//! `McpSaver`: transport MCP do trait `ng_core::Saver` (plano 004 §2b).
//!
//! Fala MCP stdio: JSON-RPC 2.0 com um objeto JSON por linha (delimitado
//! por newline — nunca framing Content-Length estilo LSP). O servidor é
//! lançado POR CHAMADA (`spec.command` é o argv), com o mesmo envelope de
//! segurança do `SubprocessSaver`:
//! - Zero shell: spawn direto por argv array; conteúdo trafega SÓ dentro
//!   do JSON-RPC via stdin, nunca em linha de comando.
//! - Ambiente mínimo no filho (`env_clear` + só `PATH`/`HOME`).
//! - `timeout_ms` estrito englobando handshake + tools/call, com SIGKILL
//!   no estouro; caps de bytes de entrada/saída estritos.
//! - Qualquer desvio do protocolo (exit precoce, JSON inválido, id
//!   inesperado, `isError`) vira `Err` — e o chamador trata `Err` como
//!   pass-through byte-idêntico do conteúdo original. Falhar é sempre
//!   seguro.
//!
//! Sequência por chamada: `initialize` (id 1) → resposta → notificação
//! `notifications/initialized` + `tools/call` (id 2) → resposta →
//! SIGKILL. O worker do `ngd` é serial e os itens são poucos; manter o
//! servidor vivo entre chamadas seria otimização sem necessidade e com
//! superfície de estado a mais.
//!
//! Convenção de resposta da tool de compress (documentada no template do
//! `savers.toml`): os blocos `{type:"text"}` de `result.content` são
//! concatenados; se o texto resultante for o JSON `{"text": ..., "ref":
//! ...}` vale o mesmo contrato do CLI (2a); senão o texto cru inteiro é o
//! digest, sem ref. Tokens SEMPRE pela nossa heurística, via
//! `Compressed::from_input` — número auto-reportado do saver não existe
//! neste fluxo.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ng_core::saver::{Compressed, Saver, SaverRef};

use crate::saver_cli::CompressReply;
use crate::savers::{McpTools, SaverSpec, Transport};
use crate::{Error, Result};

const PROTOCOL_VERSION: &str = "2024-11-05";
const INIT_ID: i64 = 1;
const CALL_ID: i64 = 2;

/// Impl do contrato MCP 2b sobre o trait `Saver`.
pub struct McpSaver {
    spec: SaverSpec,
}

impl McpSaver {
    /// Constrói a partir de um [`SaverSpec`] com `transport = "mcp"` e
    /// `tools` presente (o parse do `savers.toml` já garante os dois; aqui
    /// é defesa contra specs montadas à mão).
    pub fn new(spec: SaverSpec) -> Result<Self> {
        if spec.transport != Transport::Mcp {
            return Err(Error::Other(format!(
                "saver {}: spec cli entregue ao transporte MCP",
                spec.name
            )));
        }
        if spec.tools.is_none() {
            return Err(Error::Other(format!(
                "saver {}: transport mcp exige tools.compress",
                spec.name
            )));
        }
        Ok(Self { spec })
    }

    pub fn spec(&self) -> &SaverSpec {
        &self.spec
    }

    fn tools(&self) -> &McpTools {
        self.spec.tools.as_ref().expect("validado em new")
    }

    /// Uma sessão MCP completa (spawn → handshake → tools/call → SIGKILL),
    /// devolvendo o texto concatenado de `result.content`.
    fn call_tool(
        &self,
        tool: &str,
        arguments: serde_json::Value,
        max_output: usize,
    ) -> ng_core::Result<String> {
        mcp_call(
            &self.spec.command,
            tool,
            arguments,
            Duration::from_millis(self.spec.timeout_ms),
            max_output,
        )
        .map_err(|e| ng_core::Error::Other(format!("saver {}: {e}", self.spec.name)))
    }
}

impl Saver for McpSaver {
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
        let arguments = serde_json::json!({ "content": input, "budget_tokens": budget });
        let text = self.call_tool(
            &self.tools().compress,
            arguments,
            self.spec.max_output_bytes,
        )?;
        // Convenção documentada no template: JSON {"text","ref"} (contrato
        // CLI) quando parsear; senão o texto cru inteiro é o digest.
        let (digest, reference) = match serde_json::from_str::<CompressReply>(&text) {
            Ok(reply) => (reply.text, reply.reference),
            Err(_) => (text, None),
        };
        let reversible_ref = match reference {
            Some(key) => Some(SaverRef::new(&self.spec.name, &key)?),
            None => None,
        };
        Compressed::from_input(input, digest, budget, reversible_ref)
    }

    fn retrieve(&self, r: &SaverRef) -> ng_core::Result<String> {
        if r.saver != self.spec.name {
            return Err(ng_core::Error::Other(format!(
                "ref de outro saver ({}) entregue a {}",
                r.saver, self.spec.name
            )));
        }
        let Some(tool) = self.tools().retrieve.clone() else {
            return Err(ng_core::Error::Other(format!(
                "saver {} não define tools.retrieve — recupere pelo banco ng",
                self.spec.name
            )));
        };
        let arguments = serde_json::json!({ "ref": r.key });
        // O original tem no máximo max_input_bytes, mas volta embrulhado
        // em JSON-RPC com escaping de string (pior caso ~2×) + envelope +
        // resposta do handshake: cap com folga explícita, nunca ilimitado.
        let cap = self
            .spec
            .max_input_bytes
            .saturating_mul(2)
            .saturating_add(4096);
        self.call_tool(&tool, arguments, cap)
    }
}

/// Eventos da thread leitora de stdout para a thread da sessão.
enum ReadEvent {
    /// Uma linha completa (sem o `\n`), possivelmente lossy em UTF-8 — o
    /// parse JSON decide se presta.
    Line(String),
    /// Total de bytes lidos passou do cap: falha, o processo vai morrer.
    OverCap(usize),
    Eof,
    Failed(std::io::Error),
}

/// Sessão MCP inteira contra um servidor recém-spawnado. O processo morre
/// SEMPRE no fim (SIGKILL), sucesso ou falha — ciclo de vida por-chamada.
fn mcp_call(
    argv: &[String],
    tool: &str,
    arguments: serde_json::Value,
    timeout: Duration,
    max_output: usize,
) -> Result<String> {
    let Some((program, args)) = argv.split_first() else {
        return Err(Error::Other("argv vazio".into()));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Ambiente limpo: nada do env do daemon vaza para o servidor MCP;
        // PATH/HOME voltam porque são o mínimo para um binário funcionar.
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

    // Flag de encerramento das threads de I/O: com os fds não-bloqueantes
    // (ver `set_nonblocking`), reader e writer observam a flag em vez de
    // dormirem para sempre num pipe que um neto do servidor (double-fork)
    // mantém aberto depois do SIGKILL no filho direto.
    let done = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().expect("stdout piped acima");
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn({
        let done = Arc::clone(&done);
        move || read_lines(stdout, max_output, tx, done)
    });
    let mut payload_writer = None;

    let result = drive_session(
        &mut child,
        &rx,
        &done,
        &mut payload_writer,
        tool,
        arguments,
        timeout,
    );
    // SIGKILL incondicional: o servidor é por-chamada; não esperamos
    // shutdown gracioso de um processo não-confiável.
    let _ = child.kill();
    let _ = child.wait();
    // Nenhuma thread sobrevive à chamada: a flag acorda reader/writer em no
    // máximo um tick de poll, então estes joins são finitos mesmo com um
    // neto segurando as pontas dos pipes. Sem isso, o worker serial do
    // `ngd` acumularia uma thread leitora bloqueada por chamada.
    done.store(true, Ordering::Relaxed);
    let _ = reader.join();
    if let Some(writer) = payload_writer {
        let _ = writer.join();
    }
    result
}

fn drive_session(
    child: &mut Child,
    rx: &Receiver<ReadEvent>,
    done: &Arc<AtomicBool>,
    payload_writer: &mut Option<std::thread::JoinHandle<()>>,
    tool: &str,
    arguments: serde_json::Value,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;

    let mut stdin = child.stdin.take().expect("stdin piped acima");
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": INIT_ID,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "not-goldfish", "version": env!("CARGO_PKG_VERSION") },
        },
    });
    // Mensagem pequena e fixa: cabe no buffer do pipe, write direto não
    // bloqueia mesmo que o filho nunca leia.
    let mut init_line = init.to_string();
    init_line.push('\n');
    stdin
        .write_all(init_line.as_bytes())
        .map_err(|e| Error::Other(format!("escrevendo initialize: {e}")))?;

    let init_reply = recv_response(rx, deadline, INIT_ID)?;
    if init_reply.get("result").is_none() {
        return Err(Error::Other(
            "initialize sem result — o servidor recusou o handshake".into(),
        ));
    }

    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": CALL_ID,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let mut payload =
        String::from("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
    payload.push_str(&call.to_string());
    payload.push('\n');
    // O tools/call carrega o conteúdo inteiro e pode passar do buffer do
    // pipe: escrita em thread própria para um filho que não lê não furar o
    // deadline. Broken pipe não é erro nosso — a resposta (ou a falta
    // dela) decide. O drop fecha o pipe: o servidor vê EOF depois do call.
    let payload = payload.into_bytes();
    *payload_writer = Some(std::thread::spawn({
        let done = Arc::clone(done);
        move || {
            // Mesmo racional do reader: com stdin não-bloqueante a thread
            // observa a flag `done` em vez de dormir num write contra um
            // pipe cheio cuja ponta de leitura um neto mantém aberta. Se o
            // fcntl falhar (não deveria num fd válido), o write_all
            // bloqueante de antes é o fallback — nunca pior que o antigo.
            if set_nonblocking(&stdin).is_ok() {
                write_all_nonblocking(&mut stdin, &payload, &done);
            } else {
                let _ = stdin.write_all(&payload);
            }
            drop(stdin);
        }
    }));

    let reply = recv_response(rx, deadline, CALL_ID)?;
    extract_text(&reply)
}

/// Coloca o fd em modo não-bloqueante — pré-condição para as threads de
/// I/O poderem observar a flag `done` em vez de dormirem para sempre num
/// read/write contra um pipe que um neto do servidor mantém aberto. Afeta
/// só a nossa ponta do pipe; a ponta do servidor segue bloqueante normal.
fn set_nonblocking(fd: &impl AsRawFd) -> std::io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Intervalo de poll das threads de I/O não-bloqueantes entre tentativas —
/// também é o atraso máximo entre `done` ser setada e a thread sair.
const IO_POLL: Duration = Duration::from_millis(5);

/// `write_all` cooperativo sobre um fd não-bloqueante: para de escrever
/// assim que `done` for setada ou o pipe quebrar. Erro não é reportado —
/// como no write bloqueante de antes, a resposta (ou a falta dela) decide.
fn write_all_nonblocking(stdin: &mut ChildStdin, bytes: &[u8], done: &AtomicBool) {
    let mut written = 0;
    while written < bytes.len() {
        if done.load(Ordering::Relaxed) {
            return;
        }
        match stdin.write(&bytes[written..]) {
            Ok(0) => return,
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(IO_POLL),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Thread leitora: chunks crus de stdout com cap estrito no TOTAL de bytes
/// (nunca bufferiza uma linha gigante inteira antes de checar o cap),
/// repartidos em linhas para o canal. Leitura não-bloqueante em loop de
/// poll: quando a sessão termina e seta `done`, a thread sai em no máximo
/// um tick de [`IO_POLL`] mesmo que um neto do servidor nunca feche o pipe.
fn read_lines(
    mut stdout: ChildStdout,
    max_output: usize,
    tx: Sender<ReadEvent>,
    done: Arc<AtomicBool>,
) {
    if let Err(e) = set_nonblocking(&stdout) {
        let _ = tx.send(ReadEvent::Failed(e));
        return;
    }
    let mut pending: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        if done.load(Ordering::Relaxed) {
            return;
        }
        match stdout.read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(ReadEvent::Eof);
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(IO_POLL);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(n) => {
                total += n;
                if total > max_output {
                    let _ = tx.send(ReadEvent::OverCap(max_output));
                    return;
                }
                pending.extend_from_slice(&chunk[..n]);
                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = pending.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    if tx.send(ReadEvent::Line(line)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(ReadEvent::Failed(e));
                return;
            }
        }
    }
}

/// Espera a resposta JSON-RPC com o id esperado até o deadline.
/// Notificações do servidor (sem `id`) são ignoradas; um `id` diferente do
/// esperado é desvio de protocolo (o cliente não pipelina outros requests)
/// e vira `Err`.
fn recv_response(
    rx: &Receiver<ReadEvent>,
    deadline: Instant,
    expected: i64,
) -> Result<serde_json::Value> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(Error::Other(format!(
                "timeout esperando resposta id {expected} — processo morto (SIGKILL)"
            )));
        };
        let event = match rx.recv_timeout(remaining) {
            Ok(event) => event,
            Err(_) => {
                return Err(Error::Other(format!(
                    "timeout esperando resposta id {expected} — processo morto (SIGKILL)"
                )))
            }
        };
        match event {
            ReadEvent::Line(line) => {
                let value: serde_json::Value = serde_json::from_str(&line).map_err(|e| {
                    Error::Other(format!("linha de stdout não é JSON-RPC válido: {e}"))
                })?;
                match value.get("id") {
                    None => continue, // notificação do servidor: dado, ignorado
                    Some(id) if id.as_i64() == Some(expected) => return Ok(value),
                    Some(other) => {
                        return Err(Error::Other(format!(
                            "id inesperado na resposta: {other} (esperado {expected})"
                        )))
                    }
                }
            }
            ReadEvent::OverCap(cap) => {
                return Err(Error::Other(format!("saída acima do cap de {cap} bytes")))
            }
            ReadEvent::Eof => {
                return Err(Error::Other(format!(
                    "stdout fechou antes da resposta id {expected}"
                )))
            }
            ReadEvent::Failed(e) => return Err(Error::Other(format!("lendo stdout: {e}"))),
        }
    }
}

/// Extrai e concatena os blocos `{type:"text"}` de `result.content` de uma
/// resposta de `tools/call`. `error` JSON-RPC ou `result.isError = true`
/// são falha (pass-through no chamador).
fn extract_text(reply: &serde_json::Value) -> Result<String> {
    if let Some(error) = reply.get("error") {
        return Err(Error::Other(format!(
            "tools/call devolveu erro JSON-RPC: {error}"
        )));
    }
    let result = reply
        .get("result")
        .ok_or_else(|| Error::Other("resposta de tools/call sem result".into()))?;
    if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        return Err(Error::Other("tools/call respondeu isError = true".into()));
    }
    let content = result
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Other("result.content ausente ou não é array".into()))?;
    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                out.push_str(text);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefixo comum dos servidores MCP fake em sh: responde ao
    /// `initialize`, consome a notificação e lê o `tools/call` em `$call`.
    /// `printf '%s\n'` de propósito: o argumento não sofre processamento
    /// de escapes, então os `\"` do JSON aninhado saem literais.
    const HANDSHAKE: &str = r#"read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}'
read -r _notif
read -r call
"#;

    /// Spec MCP de teste sobre um script sh (escolha da config de teste,
    /// como seria de um usuário — produção nunca embrulha nada em shell).
    fn mcp_spec(server_script: &str) -> SaverSpec {
        SaverSpec {
            name: "fakemcp".into(),
            enabled: true,
            transport: Transport::Mcp,
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("{HANDSHAKE}{server_script}"),
            ],
            retrieve_command: Vec::new(),
            timeout_ms: 2000,
            max_input_bytes: 4096,
            max_output_bytes: 4096,
            budget_tokens: 64,
            apply_to: vec!["tool_output".into()],
            tools: Some(McpTools {
                compress: "fake_compress".into(),
                retrieve: Some("fake_retrieve".into()),
            }),
        }
    }

    #[test]
    fn compress_plain_text_reply_uses_our_token_counts() {
        // Resposta em texto puro (sem o JSON do contrato CLI): o texto cru
        // é o digest, ref = None, tokens SEMPRE da nossa heurística.
        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"digest de teste"}]}}'"#,
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
        assert!(c.reversible_ref.is_none());
    }

    #[test]
    fn compress_json_reply_yields_ref_like_cli_contract() {
        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"text\":\"digest\",\"ref\":\"abc123\"}"}]}}'"#,
        ))
        .unwrap();
        let c = saver.compress(&"x".repeat(400), 64).unwrap();
        assert_eq!(c.text, "digest");
        assert_eq!(
            c.reversible_ref.as_ref().unwrap().to_string(),
            "fakemcp:abc123"
        );
    }

    #[test]
    fn compress_concatenates_multiple_text_blocks() {
        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"parte1 "},{"type":"image","data":"x"},{"type":"text","text":"parte2"}]}}'"#,
        ))
        .unwrap();
        let c = saver.compress(&"x".repeat(400), 64).unwrap();
        assert_eq!(c.text, "parte1 parte2");
    }

    #[test]
    fn content_and_budget_travel_inside_jsonrpc_stdin_never_argv() {
        // O servidor só vê o conteúdo se ele chegou na linha do tools/call
        // via stdin — o argv é fixo e não contém input em lugar nenhum.
        let saver = McpSaver::new(mcp_spec(
            r#"case "$call" in
  *conteudo-sentinela-1234567890*) c=viu-stdin;;
  *) c=sem-stdin;;
esac
case "$call" in
  *'"budget_tokens":42'*) b=viu-budget;;
  *) b=sem-budget;;
esac
printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"%s %s"}]}}\n' "$c" "$b""#,
        ))
        .unwrap();
        let c = saver.compress("conteudo-sentinela-1234567890", 42).unwrap();
        assert_eq!(c.text, "viu-stdin viu-budget");
    }

    #[test]
    fn is_error_true_is_an_error() {
        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"isError":true,"content":[{"type":"text","text":"boom"}]}}'"#,
        ))
        .unwrap();
        let err = saver.compress(&"z".repeat(100), 64);
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("isError"));
    }

    #[test]
    fn jsonrpc_error_and_unexpected_id_are_errors() {
        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no such tool"}}'"#,
        ))
        .unwrap();
        assert!(saver.compress(&"z".repeat(100), 64).is_err());

        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":99,"result":{"content":[]}}'"#,
        ))
        .unwrap();
        let err = saver.compress(&"z".repeat(100), 64);
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("id inesperado"));
    }

    #[test]
    fn server_notifications_without_id_are_ignored() {
        let saver = McpSaver::new(mcp_spec(
            r#"printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"ok"}]}}'"#,
        ))
        .unwrap();
        assert_eq!(saver.compress(&"z".repeat(100), 64).unwrap().text, "ok");
    }

    /// Threads vivas neste processo via /proc (Linux). Fora do Linux
    /// devolve 0 e o assert de contagem vira no-op — o assert de tempo
    /// finito continua valendo.
    fn count_threads() -> usize {
        std::fs::read_dir("/proc/self/task")
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn double_forking_server_leaves_no_stuck_thread_behind() {
        // O servidor responde e sai, mas um neto em background herda os
        // pipes e nunca os fecha. Antes, cada chamada vazava uma thread
        // leitora bloqueada em read() para sempre; agora a flag `done` +
        // fds não-bloqueantes garantem join finito de todas as threads.
        let saver = McpSaver::new(mcp_spec(
            r#"( sleep 30 ) &
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"ok"}]}}'"#,
        ))
        .unwrap();
        let threads_before = count_threads();
        let t0 = Instant::now();
        for _ in 0..8 {
            assert_eq!(saver.compress(&"z".repeat(100), 64).unwrap().text, "ok");
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "8 chamadas não podem esperar netos: {}ms",
            t0.elapsed().as_millis()
        );
        let threads_after = count_threads();
        // Margem para threads de outros testes do mesmo processo indo e
        // vindo em paralelo; sem o fix, as 8 chamadas vazariam 8 threads.
        assert!(
            threads_after <= threads_before + 3,
            "threads vazadas: antes={threads_before} depois={threads_after}"
        );
    }

    #[test]
    fn timeout_kills_the_process_fast() {
        // Servidor que nunca responde nem ao initialize.
        let mut spec = mcp_spec("");
        spec.command = vec!["sh".into(), "-c".into(), "sleep 30".into()];
        spec.timeout_ms = 100;
        let saver = McpSaver::new(spec).unwrap();
        let t0 = Instant::now();
        let err = saver.compress(&"z".repeat(100), 64);
        assert!(err.is_err());
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "timeout de 100ms não pode levar {}ms",
            t0.elapsed().as_millis()
        );
        assert!(format!("{}", err.unwrap_err()).contains("timeout"));
    }

    #[test]
    fn early_exit_before_reply_is_an_error() {
        let mut spec = mcp_spec("");
        spec.command = vec!["sh".into(), "-c".into(), "exit 0".into()];
        let saver = McpSaver::new(spec).unwrap();
        assert!(saver.compress(&"z".repeat(100), 64).is_err());
    }

    #[test]
    fn output_over_cap_is_an_error() {
        let mut spec = mcp_spec(r#"head -c 65536 /dev/zero | tr '\0' 'a'"#);
        spec.max_output_bytes = 1024;
        let saver = McpSaver::new(spec).unwrap();
        let err = saver.compress(&"z".repeat(100), 64);
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("cap"));
    }

    #[test]
    fn input_over_cap_is_refused_before_spawn() {
        let mut spec = mcp_spec("");
        // Comando inexistente: se spawnasse, o erro seria outro.
        spec.command = vec!["/nonexistent-mcp-server".into()];
        spec.max_input_bytes = 64;
        let saver = McpSaver::new(spec).unwrap();
        let err = saver.compress(&"z".repeat(100), 64);
        assert!(err.is_err());
        assert!(format!("{}", err.unwrap_err()).contains("pass-through"));
    }

    #[test]
    fn compress_output_over_budget_is_an_error() {
        // Digest válido mas acima do budget: ERRO via Compressed::from_input.
        let saver = McpSaver::new(mcp_spec(
            r#"big=$(head -c 400 /dev/zero | tr '\0' 'a')
printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$big""#,
        ))
        .unwrap();
        assert!(saver.compress(&"z".repeat(800), 10).is_err());
    }

    #[test]
    fn child_env_is_scrubbed() {
        std::env::set_var("NG_TEST_MCP_SECRET_XYZ", "vazou");
        let saver = McpSaver::new(mcp_spec(
            r#"printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"v=%s"}]}}\n' "${NG_TEST_MCP_SECRET_XYZ:-limpo}""#,
        ))
        .unwrap();
        let c = saver.compress(&"z".repeat(100), 64).unwrap();
        std::env::remove_var("NG_TEST_MCP_SECRET_XYZ");
        assert_eq!(c.text, "v=limpo");
    }

    #[test]
    fn retrieve_sends_ref_and_returns_raw_text() {
        // O fake confirma que a ref chegou nos arguments e devolve o
        // "original" cru em result.content.
        let saver = McpSaver::new(mcp_spec(
            r#"case "$call" in
  *'"ref":"k1"'*) out=original-completo;;
  *) out=ref-nao-chegou;;
esac
printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$out""#,
        ))
        .unwrap();
        let r = SaverRef::new("fakemcp", "k1").unwrap();
        assert_eq!(saver.retrieve(&r).unwrap(), "original-completo");
    }

    #[test]
    fn retrieve_rejects_foreign_refs_and_missing_tool() {
        let saver = McpSaver::new(mcp_spec("")).unwrap();
        let foreign = SaverRef::new("outro", "k").unwrap();
        assert!(saver.retrieve(&foreign).is_err());

        let mut spec = mcp_spec("");
        spec.tools = Some(McpTools {
            compress: "fake_compress".into(),
            retrieve: None,
        });
        let saver = McpSaver::new(spec).unwrap();
        let own = SaverRef::new("fakemcp", "k").unwrap();
        assert!(saver.retrieve(&own).is_err(), "sem tools.retrieve é erro");
    }

    #[test]
    fn new_rejects_cli_spec_and_missing_tools() {
        let mut spec = mcp_spec("");
        spec.transport = Transport::Cli;
        spec.tools = None;
        assert!(McpSaver::new(spec).is_err());

        let mut spec = mcp_spec("");
        spec.tools = None;
        assert!(McpSaver::new(spec).is_err());
    }
}
