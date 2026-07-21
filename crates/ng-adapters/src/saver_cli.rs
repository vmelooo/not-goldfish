//! `SubprocessSaver`: transport CLI do trait `ng_core::Saver` (plano 004
//! §2a).
//!
//! Comandos externos são não-confiáveis por premissa. Medidas aqui:
//! - Zero shell: spawn direto por argv array (`Command::new` + `.args`),
//!   nenhuma interpolação de conteúdo em linha de comando — conteúdo
//!   trafega só por stdin/stdout. `{budget}` é o único placeholder e é
//!   substituído por inteiro decimal.
//! - Ambiente mínimo no filho (`env_clear` + só `PATH`/`HOME`): segredos
//!   de env do daemon não vazam para o saver.
//! - `timeout_ms` estrito com SIGKILL; `max_input_bytes` /
//!   `max_output_bytes` estritos (excesso = falha do saver, nunca OOM
//!   nosso).
//! - Toda falha vira `Err` — e o chamador (worker do `ngd`, bench) trata
//!   `Err` como pass-through byte-idêntico do conteúdo original.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ng_core::saver::{Compressed, Saver, SaverRef};

use crate::savers::{SaverSpec, Transport};
use crate::{Error, Result};

/// Impl do contrato de processo 2a: compress = conteúdo via stdin, JSON de
/// uma linha em stdout; retrieve = `{"ref": ...}` via stdin, original cru
/// em stdout.
pub struct SubprocessSaver {
    spec: SaverSpec,
}

/// Resposta JSON do modo compress do processo externo. Compartilhada com o
/// transporte MCP (`saver_mcp`), que aceita o mesmo JSON dentro do bloco de
/// texto da resposta da tool.
#[derive(serde::Deserialize)]
pub(crate) struct CompressReply {
    pub(crate) text: String,
    #[serde(default, rename = "ref")]
    pub(crate) reference: Option<String>,
}

impl SubprocessSaver {
    /// Constrói a partir de um [`SaverSpec`] com `transport = "cli"`;
    /// specs MCP pertencem ao [`crate::saver_mcp::McpSaver`] — use
    /// [`build_one`] para rotear pelo transporte declarado.
    pub fn new(spec: SaverSpec) -> Result<Self> {
        if spec.transport != Transport::Cli {
            return Err(Error::Other(format!(
                "saver {}: spec mcp entregue ao transporte CLI — use build_one/McpSaver",
                spec.name
            )));
        }
        Ok(Self { spec })
    }

    pub fn spec(&self) -> &SaverSpec {
        &self.spec
    }

    /// argv com `{budget}` substituído (só por inteiro decimal — nunca por
    /// conteúdo).
    fn compress_argv(&self, budget: i64) -> Vec<String> {
        self.spec
            .command
            .iter()
            .map(|arg| arg.replace("{budget}", &budget.to_string()))
            .collect()
    }
}

impl Saver for SubprocessSaver {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn compress(&self, input: &str, budget: i64) -> ng_core::Result<Compressed> {
        if budget <= 0 {
            return Err(ng_core::Error::Other("budget deve ser > 0".into()));
        }
        if input.len() > self.spec.max_input_bytes {
            return Err(ng_core::Error::Other(format!(
                "input de {} bytes acima do cap {} — pass-through",
                input.len(),
                self.spec.max_input_bytes
            )));
        }
        let argv = self.compress_argv(budget);
        let stdout = run_capped(
            &argv,
            input.as_bytes(),
            Duration::from_millis(self.spec.timeout_ms),
            self.spec.max_output_bytes,
        )
        .map_err(|e| ng_core::Error::Other(format!("saver {}: {e}", self.spec.name)))?;
        let reply: CompressReply = serde_json::from_slice(&stdout).map_err(|e| {
            ng_core::Error::Other(format!(
                "saver {}: stdout não é o JSON do contrato: {e}",
                self.spec.name
            ))
        })?;
        let reversible_ref = match reply.reference {
            Some(key) => Some(SaverRef::new(&self.spec.name, &key)?),
            None => None,
        };
        Compressed::from_input(input, reply.text, budget, reversible_ref)
    }

    fn retrieve(&self, r: &SaverRef) -> ng_core::Result<String> {
        if r.saver != self.spec.name {
            return Err(ng_core::Error::Other(format!(
                "ref de outro saver ({}) entregue a {}",
                r.saver, self.spec.name
            )));
        }
        if self.spec.retrieve_command.is_empty() {
            return Err(ng_core::Error::Other(format!(
                "saver {} não define retrieve_command — recupere pelo banco ng",
                self.spec.name
            )));
        }
        let payload = serde_json::json!({ "ref": r.key }).to_string();
        // O original devolvido nunca é maior que o que aceitamos comprimir.
        let stdout = run_capped(
            &self.spec.retrieve_command,
            payload.as_bytes(),
            Duration::from_millis(self.spec.timeout_ms),
            self.spec.max_input_bytes,
        )
        .map_err(|e| ng_core::Error::Other(format!("saver {}: retrieve: {e}", self.spec.name)))?;
        String::from_utf8(stdout).map_err(|_| {
            ng_core::Error::Other(format!(
                "saver {}: retrieve devolveu bytes não-UTF-8",
                self.spec.name
            ))
        })
    }
}

/// Um saver habilitado pareado com o `budget_tokens` do seu spec — a forma
/// que o worker do `ngd` consome.
pub type EnabledSaver = (Box<dyn Saver>, i64);

/// Constrói o transporte certo (CLI ou MCP) para um spec, atrás de
/// `Box<dyn Saver>`. Um spec vindo do parse do `savers.toml` nunca falha
/// aqui — as invariantes por transporte (tools presente em mcp etc.) já
/// foram validadas no parse.
pub fn build_one(spec: SaverSpec) -> Result<Box<dyn Saver>> {
    match spec.transport {
        Transport::Cli => Ok(Box::new(SubprocessSaver::new(spec)?)),
        Transport::Mcp => Ok(Box::new(crate::saver_mcp::McpSaver::new(spec)?)),
    }
}

/// Constrói os savers habilitados de uma config, ambos os transportes, já
/// pareados com o `budget_tokens` de cada spec. `skipped` só carrega specs
/// realmente inválidos (impossível para config vinda do parse — defesa
/// contra specs montados à mão), para o chamador logar.
pub fn build_enabled_savers(
    config: &crate::savers::SaversConfig,
) -> (Vec<EnabledSaver>, Vec<String>) {
    let mut built = Vec::new();
    let mut skipped = Vec::new();
    for spec in &config.savers {
        if !spec.enabled {
            continue;
        }
        let budget = spec.budget_tokens;
        match build_one(spec.clone()) {
            Ok(saver) => built.push((saver, budget)),
            Err(_) => skipped.push(spec.name.clone()),
        }
    }
    (built, skipped)
}

/// Poll interval do try_wait — curto o bastante para o timeout ser
/// respeitado com folga, longo o bastante para não virar busy-wait.
const WAIT_POLL: Duration = Duration::from_millis(10);

/// Depois do exit do filho, quanto esperar pelas threads de I/O antes de
/// abandoná-las: um neto daemonizado que herdou os pipes pode nunca
/// fechá-los, e esperar EOF cegamente travaria o worker serial do `ngd`
/// para sempre. Curto de propósito — com o filho já morto, o que resta no
/// pipe é só o buffer do kernel, drenado em microssegundos.
const DRAIN_GRACE: Duration = Duration::from_millis(150);

/// O que a thread leitora de stdout acumulou até agora. Compartilhado via
/// `Arc<Mutex<..>>` (em vez de devolvido no return da thread) para que o
/// chamador possa pegar o resultado parcial e abandonar a thread se um
/// neto segurar o pipe aberto — a memória retida é limitada pelo cap.
#[derive(Default)]
struct StdoutCapture {
    buf: Vec<u8>,
    over_cap: bool,
    failed: Option<std::io::Error>,
}

/// Spawn de `argv` sem shell, com env mínimo, stdin alimentado com
/// `stdin_bytes`, stdout lido até `max_output` bytes e SIGKILL no estouro
/// de `timeout`. Qualquer desvio do contrato (exit != 0, saída acima do
/// cap, timeout) é `Err`.
fn run_capped(
    argv: &[String],
    stdin_bytes: &[u8],
    timeout: Duration,
    max_output: usize,
) -> Result<Vec<u8>> {
    let Some((program, args)) = argv.split_first() else {
        return Err(Error::Other("argv vazio".into()));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Ambiente limpo: nada do env do daemon vaza para o processo do
        // saver; PATH/HOME voltam porque são o mínimo para um binário
        // funcionar (resolver a si mesmo, achar o próprio estado).
        .env_clear();
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }

    let mut child = command
        .spawn()
        .map_err(|e| Error::Other(format!("spawn de {program}: {e}")))?;

    // stdin numa thread própria: um filho que não lê stdin encheria o pipe
    // e um write bloqueante aqui furaria o timeout. Broken pipe (filho saiu
    // sem ler tudo) não é erro nosso — o exit status decide.
    let mut stdin = child.stdin.take().expect("stdin piped acima");
    let stdin_bytes = stdin_bytes.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&stdin_bytes);
        drop(stdin); // fecha o pipe: o filho vê EOF
    });

    // stdout idem, com cap estrito: guarda no máximo max_output bytes e
    // marca o estouro sem jamais bufferizar uma saída gigante.
    let mut stdout = child.stdout.take().expect("stdout piped acima");
    let capture = Arc::new(Mutex::new(StdoutCapture::default()));
    let reader_capture = Arc::clone(&capture);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    let mut cap = reader_capture.lock().unwrap();
                    if cap.buf.len() + n > max_output {
                        cap.over_cap = true;
                        // Continua drenando (sem guardar) para o filho não
                        // travar em write, mas o resultado já é falha.
                    } else {
                        cap.buf.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(e) => {
                    reader_capture.lock().unwrap().failed = Some(e);
                    return;
                }
            }
        }
    });

    // Espera com deadline dura: estourou => SIGKILL, sem apelação.
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // NÃO fazer join das threads de I/O aqui: um neto do
                    // processo morto (ex.: `sh -c` que forkou) pode manter a
                    // ponta de escrita do pipe aberta e segurar o reader até
                    // ele mesmo sair — exatamente a espera que o timeout
                    // existe para evitar. As threads se encerram sozinhas no
                    // EOF/broken pipe; o buffer que seguram é limitado pelo
                    // cap de saída.
                    return Err(Error::Other(format!(
                        "timeout de {}ms — processo morto (SIGKILL)",
                        timeout.as_millis()
                    )));
                }
                std::thread::sleep(WAIT_POLL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Other(format!("wait falhou: {e}")));
            }
        }
    };
    // Exit 0 NÃO garante EOF nos pipes: um neto daemonizado que herdou
    // stdout/stdin mantém as pontas abertas e um join cego aqui travaria o
    // worker serial para sempre. As threads de I/O ganham um grace curto
    // para terminar; passado ele, são abandonadas e o que a leitora já
    // capturou é a resposta (a saída de um neto não faz parte do contrato).
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    while (!reader.is_finished() || !writer.is_finished()) && Instant::now() < drain_deadline {
        std::thread::sleep(WAIT_POLL);
    }
    if reader.is_finished() {
        reader
            .join()
            .map_err(|_| Error::Other("thread de leitura de stdout morreu".into()))?;
    }
    let (buf, over_cap) = {
        let mut cap = capture.lock().unwrap();
        if let Some(e) = cap.failed.take() {
            return Err(Error::Other(format!("lendo stdout: {e}")));
        }
        (std::mem::take(&mut cap.buf), cap.over_cap)
    };

    if over_cap {
        return Err(Error::Other(format!(
            "saída acima do cap de {max_output} bytes"
        )));
    }
    if !status.success() {
        return Err(Error::Other(format!("processo saiu com {status}")));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::savers::SaversConfig;

    /// Spec de teste apontando para um script sh. O argv array com
    /// `sh -c <script>` aqui é uma ESCOLHA da config de teste (como seria
    /// de um usuário) — o código de produção nunca envolve conteúdo nem
    /// embrulha nada em shell por conta própria.
    fn spec_with(script: &str, retrieve_script: Option<&str>) -> SaverSpec {
        SaverSpec {
            name: "fake".into(),
            enabled: true,
            transport: Transport::Cli,
            command: vec!["sh".into(), "-c".into(), script.into()],
            retrieve_command: retrieve_script
                .map(|s| vec!["sh".into(), "-c".into(), s.into()])
                .unwrap_or_default(),
            timeout_ms: 2000,
            max_input_bytes: 4096,
            max_output_bytes: 1024,
            budget_tokens: 64,
            apply_to: vec!["tool_output".into()],
            tools: None,
        }
    }

    #[test]
    fn compress_happy_path_uses_our_token_counts() {
        // O "saver" alega nada — devolve um digest e uma ref; os tokens
        // saem da nossa heurística, calculados sobre input/saída reais.
        let saver = SubprocessSaver::new(spec_with(
            r#"cat > /dev/null; printf '{"text": "digest de teste", "ref": "abc123"}'"#,
            None,
        ))
        .unwrap();
        let input = "x".repeat(400);
        let c = saver.compress(&input, 64).unwrap();
        assert_eq!(c.text, "digest de teste");
        assert_eq!(c.tokens_before, 100);
        assert_eq!(
            c.tokens_after,
            ng_core::saver::estimate_tokens("digest de teste")
        );
        assert_eq!(
            c.reversible_ref.as_ref().unwrap().to_string(),
            "fake:abc123"
        );
    }

    #[test]
    fn compress_receives_content_via_stdin_never_argv() {
        // O script ecoa o stdin dentro do JSON: prova que o conteúdo
        // trafega por stdin (o argv não contém o input em lugar nenhum).
        let saver =
            SubprocessSaver::new(spec_with(r#"c=$(cat); printf '{"text": "%s"}' "$c""#, None))
                .unwrap();
        let c = saver.compress("conteudo-sentinela-1234567890", 64).unwrap();
        assert_eq!(c.text, "conteudo-sentinela-1234567890");
    }

    #[test]
    fn budget_placeholder_is_substituted_with_integer_only() {
        let mut spec = spec_with("", None);
        spec.command = vec![
            "sh".into(),
            "-c".into(),
            r#"cat > /dev/null; printf '{"text": "b=%s"}' "$0""#.into(),
            "{budget}".into(),
        ];
        let saver = SubprocessSaver::new(spec).unwrap();
        let c = saver.compress(&"y".repeat(100), 42).unwrap();
        assert_eq!(c.text, "b=42");
    }

    #[test]
    fn nonzero_exit_is_an_error_for_passthrough() {
        let saver = SubprocessSaver::new(spec_with("cat > /dev/null; exit 3", None)).unwrap();
        assert!(saver.compress(&"z".repeat(100), 64).is_err());
    }

    #[test]
    fn invalid_json_is_an_error() {
        let saver =
            SubprocessSaver::new(spec_with("cat > /dev/null; echo nao-e-json", None)).unwrap();
        assert!(saver.compress(&"z".repeat(100), 64).is_err());
    }

    #[test]
    fn timeout_kills_the_process() {
        let mut spec = spec_with("sleep 30", None);
        spec.timeout_ms = 100;
        let saver = SubprocessSaver::new(spec).unwrap();
        let t0 = std::time::Instant::now();
        let err = saver.compress(&"z".repeat(100), 64);
        assert!(err.is_err());
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "timeout de 100ms não pode levar {}ms",
            t0.elapsed().as_millis()
        );
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("timeout"), "{msg}");
    }

    #[test]
    fn daemonized_grandchild_holding_stdout_does_not_hang_run_capped() {
        // O filho responde e sai 0, mas deixa um neto (sleep em background)
        // segurando a ponta de escrita de stdout — o pipe nunca chega ao
        // EOF. Um join cego no reader esperaria os 30s do neto; o grace
        // devolve o que já foi capturado em ~150ms.
        let saver = SubprocessSaver::new(spec_with(
            r#"cat > /dev/null; ( sleep 30 ) & printf '{"text": "ok"}'"#,
            None,
        ))
        .unwrap();
        let t0 = std::time::Instant::now();
        let c = saver.compress(&"z".repeat(100), 64).unwrap();
        assert_eq!(c.text, "ok");
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "não pode esperar o neto: {}ms",
            t0.elapsed().as_millis()
        );
    }

    #[test]
    fn output_over_cap_is_an_error() {
        let mut spec = spec_with(
            // ~64 KiB de saída com cap de 1 KiB.
            r#"cat > /dev/null; head -c 65536 /dev/zero | tr '\0' 'a'"#,
            None,
        );
        spec.max_output_bytes = 1024;
        let saver = SubprocessSaver::new(spec).unwrap();
        let err = saver.compress(&"z".repeat(100), 10_000);
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("cap"));
    }

    #[test]
    fn input_over_cap_is_refused_before_spawn() {
        let mut spec = spec_with(r#"cat > /dev/null; printf '{"text": "x"}'"#, None);
        spec.max_input_bytes = 64;
        let saver = SubprocessSaver::new(spec).unwrap();
        assert!(saver.compress(&"z".repeat(100), 64).is_err());
    }

    #[test]
    fn compress_output_over_budget_is_an_error() {
        // Saída válida mas maior que o budget: o contrato diz ERRO, não
        // "quase" — imposto por Compressed::from_input, nunca pelo saver.
        let saver = SubprocessSaver::new(spec_with(
            r#"cat > /dev/null; printf '{"text": "%s"}' "$(head -c 400 /dev/zero | tr '\0' 'a')""#,
            None,
        ))
        .unwrap();
        assert!(saver.compress(&"z".repeat(800), 10).is_err());
    }

    #[test]
    fn child_env_is_scrubbed() {
        // Um segredo no env do pai não pode aparecer no filho.
        std::env::set_var("NG_TEST_SECRET_XYZ", "vazou");
        let saver = SubprocessSaver::new(spec_with(
            r#"cat > /dev/null; printf '{"text": "v=%s"}' "${NG_TEST_SECRET_XYZ:-limpo}""#,
            None,
        ))
        .unwrap();
        let c = saver.compress(&"z".repeat(100), 64).unwrap();
        std::env::remove_var("NG_TEST_SECRET_XYZ");
        assert_eq!(c.text, "v=limpo");
    }

    #[test]
    fn retrieve_roundtrips_via_stdin_json() {
        let saver = SubprocessSaver::new(spec_with(
            r#"cat > /dev/null; printf '{"text": "d", "ref": "k1"}'"#,
            // O retrieve fake devolve o payload que recebeu — suficiente
            // para provar o formato {"ref": ...} via stdin e o stdout cru.
            Some("cat"),
        ))
        .unwrap();
        let r = SaverRef::new("fake", "k1").unwrap();
        let out = saver.retrieve(&r).unwrap();
        assert_eq!(out, r#"{"ref":"k1"}"#);
    }

    #[test]
    fn retrieve_rejects_foreign_refs_and_missing_command() {
        let saver = SubprocessSaver::new(spec_with("cat", None)).unwrap();
        let foreign = SaverRef::new("outro", "k").unwrap();
        assert!(saver.retrieve(&foreign).is_err());
        let own = SaverRef::new("fake", "k").unwrap();
        assert!(saver.retrieve(&own).is_err(), "sem retrieve_command é erro");
    }

    #[test]
    fn build_enabled_savers_builds_both_transports_and_skips_disabled() {
        let config = SaversConfig::from_global_toml(
            r#"
            [savers.cli-on]
            enabled = true
            transport = "cli"
            command = ["cat"]
            budget_tokens = 32

            [savers.cli-off]
            enabled = false
            transport = "cli"
            command = ["cat"]

            [savers.mcp-on]
            enabled = true
            transport = "mcp"
            command = ["npx", "x"]
            tools = { compress = "c" }
            budget_tokens = 128
            "#,
        )
        .unwrap();
        let (built, skipped) = build_enabled_savers(&config);
        assert!(
            skipped.is_empty(),
            "mcp válido agora é construído: {skipped:?}"
        );
        assert_eq!(built.len(), 2);
        let mut names: Vec<(&str, i64)> = built
            .iter()
            .map(|(s, budget)| (s.name(), *budget))
            .collect();
        names.sort();
        assert_eq!(names, vec![("cli-on", 32), ("mcp-on", 128)]);
    }
}
