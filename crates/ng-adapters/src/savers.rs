//! Parsing do `savers.toml` (plano 004 §2c): declaração dos compressores
//! externos plugáveis.
//!
//! Regra de segurança central: comandos só podem ser definidos no arquivo
//! GLOBAL (`~/.not-goldfish/savers.toml`, escrito pelo usuário). O arquivo
//! de projeto (`.ng/config.toml`, que pode vir de um clone não-confiável)
//! só liga/desliga e ajusta budget de savers já definidos globalmente —
//! ver [`SaversConfig::apply_project_toggles`]. Um repo malicioso no
//! máximo liga um saver que o usuário já instalou de propósito.

use crate::{Error, Result};

/// Transporte de um saver. Ambos plugam atrás do trait `ng_core::Saver`:
/// `cli` = binário no contrato stdin/stdout 2a (`SubprocessSaver`);
/// `mcp` = servidor MCP stdio JSON-RPC 2.0 lançado por-chamada
/// (`McpSaver`, plano 004 §2b) — exige a tabela `[savers.<nome>.tools]`
/// com o nome da tool de compress (e opcionalmente a de retrieve).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Cli,
    Mcp,
}

/// Nomes das tools MCP de um saver `transport = "mcp"`
/// (`[savers.<nome>.tools]`). `compress` é obrigatória; `retrieve` é
/// opcional (sem ela, a recuperação continua existindo: o banco ng).
#[derive(Debug, Clone, PartialEq)]
pub struct McpTools {
    pub compress: String,
    pub retrieve: Option<String>,
}

/// Um saver declarado no `savers.toml` global.
#[derive(Debug, Clone, PartialEq)]
pub struct SaverSpec {
    pub name: String,
    pub enabled: bool,
    pub transport: Transport,
    /// argv array — NUNCA uma string shell. `{budget}` é o único
    /// placeholder aceito (substituído por inteiro decimal, jamais por
    /// conteúdo).
    pub command: Vec<String>,
    /// argv array do modo retrieve (contrato CLI 2a); vazio = saver sem
    /// retrieve próprio (a recuperação continua existindo: o banco ng).
    pub retrieve_command: Vec<String>,
    pub timeout_ms: u64,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    /// Budget do digest em tokens (heurística ×4 bytes nossa).
    pub budget_tokens: i64,
    /// Kinds de evento elegíveis. `"prompt"` é recusado no parse — prompts
    /// do usuário nunca são enviados a um processo externo.
    pub apply_to: Vec<String>,
    /// Tools MCP (`[savers.<nome>.tools]`). Obrigatório quando
    /// `transport = "mcp"`; proibido em `cli` (erro de parse, não é
    /// ignorado silenciosamente).
    pub tools: Option<McpTools>,
}

const DEFAULT_TIMEOUT_MS: u64 = 2000;
const DEFAULT_MAX_INPUT_BYTES: usize = 262_144;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 65_536;
const DEFAULT_BUDGET_TOKENS: i64 = 256;

/// Config completa: todos os savers declarados no arquivo global.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SaversConfig {
    pub savers: Vec<SaverSpec>,
}

impl SaversConfig {
    /// Parse do `savers.toml` GLOBAL (padrão `DispatchConfig::from_toml`:
    /// erro de schema é `Err` claro, nunca fallback silencioso; arquivo
    /// ausente é responsabilidade do chamador e significa "nenhum saver").
    pub fn from_global_toml(input: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(input)?;
        let mut savers = Vec::new();
        let Some(table) = value.get("savers").and_then(|v| v.as_table()) else {
            return Ok(Self { savers });
        };
        for (name, spec) in table {
            let Some(spec) = spec.as_table() else {
                return Err(Error::Other(format!(
                    "savers.toml: [savers.{name}] deve ser uma tabela"
                )));
            };
            savers.push(parse_spec(name, spec)?);
        }
        savers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { savers })
    }

    /// Aplica a seção `[savers]` do arquivo de PROJETO (`.ng/config.toml`).
    /// Só `enabled` e `budget_tokens` de savers já definidos globalmente;
    /// qualquer outra chave — em especial `command` — é ERRO, porque um
    /// repo clonado não pode contrabandear execução de comando. Nomes que
    /// não existem no global são ignorados (não criam savers novos).
    pub fn apply_project_toggles(&mut self, input: &str) -> Result<()> {
        let value: toml::Value = toml::from_str(input)?;
        let Some(table) = value.get("savers").and_then(|v| v.as_table()) else {
            return Ok(());
        };
        for (name, spec) in table {
            let Some(spec) = spec.as_table() else {
                return Err(Error::Other(format!(
                    "config de projeto: [savers.{name}] deve ser uma tabela"
                )));
            };
            for key in spec.keys() {
                if key != "enabled" && key != "budget_tokens" {
                    return Err(Error::Other(format!(
                        "config de projeto: [savers.{name}].{key} não é permitido — \
                         um arquivo de repositório só pode ligar/desligar savers \
                         definidos em ~/.not-goldfish/savers.toml (nunca definir '{key}')"
                    )));
                }
            }
            let Some(target) = self.savers.iter_mut().find(|s| s.name == *name) else {
                continue; // não definido globalmente — nunca criado por projeto
            };
            if let Some(enabled) = spec.get("enabled") {
                target.enabled = enabled.as_bool().ok_or_else(|| {
                    Error::Other(format!(
                        "config de projeto: savers.{name}.enabled deve ser bool"
                    ))
                })?;
            }
            if let Some(budget) = spec.get("budget_tokens") {
                let b = budget.as_integer().ok_or_else(|| {
                    Error::Other(format!(
                        "config de projeto: savers.{name}.budget_tokens deve ser inteiro"
                    ))
                })?;
                if b <= 0 {
                    return Err(Error::Other(format!(
                        "config de projeto: savers.{name}.budget_tokens deve ser > 0"
                    )));
                }
                target.budget_tokens = b;
            }
        }
        Ok(())
    }
}

fn parse_spec(name: &str, spec: &toml::value::Table) -> Result<SaverSpec> {
    let transport = match spec.get("transport").and_then(|v| v.as_str()) {
        Some("cli") => Transport::Cli,
        Some("mcp") => Transport::Mcp,
        Some(other) => {
            return Err(Error::Other(format!(
                "savers.toml: savers.{name}.transport desconhecido: {other} (use cli ou mcp)"
            )))
        }
        None => {
            return Err(Error::Other(format!(
                "savers.toml: savers.{name}.transport é obrigatório (cli ou mcp)"
            )))
        }
    };

    let command = string_array(name, spec, "command")?.ok_or_else(|| {
        Error::Other(format!(
            "savers.toml: savers.{name}.command é obrigatório (array argv, nunca string shell)"
        ))
    })?;
    if command.is_empty() {
        return Err(Error::Other(format!(
            "savers.toml: savers.{name}.command não pode ser vazio"
        )));
    }
    let retrieve_command = string_array(name, spec, "retrieve_command")?.unwrap_or_default();

    let apply_to =
        string_array(name, spec, "apply_to")?.unwrap_or_else(|| vec!["tool_output".to_string()]);
    if apply_to.iter().any(|k| k == "prompt") {
        return Err(Error::Other(format!(
            "savers.toml: savers.{name}.apply_to não aceita \"prompt\" — prompts do \
             usuário nunca vão para um processo externo"
        )));
    }

    let tools = parse_tools(name, spec)?;
    match transport {
        Transport::Mcp if tools.is_none() => {
            return Err(Error::Other(format!(
                "savers.toml: savers.{name} com transport = \"mcp\" exige a tabela \
                 [savers.{name}.tools] com compress = \"<tool>\" (e retrieve opcional)"
            )));
        }
        Transport::Cli if tools.is_some() => {
            return Err(Error::Other(format!(
                "savers.toml: savers.{name}.tools só faz sentido com transport = \"mcp\" — \
                 o transporte cli usa command/retrieve_command"
            )));
        }
        _ => {}
    }

    let int_field = |key: &str, default: i64| -> Result<i64> {
        match spec.get(key) {
            None => Ok(default),
            Some(v) => v.as_integer().filter(|n| *n > 0).ok_or_else(|| {
                Error::Other(format!(
                    "savers.toml: savers.{name}.{key} deve ser inteiro positivo"
                ))
            }),
        }
    };

    Ok(SaverSpec {
        name: name.to_string(),
        enabled: spec
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        transport,
        command,
        retrieve_command,
        timeout_ms: int_field("timeout_ms", DEFAULT_TIMEOUT_MS as i64)? as u64,
        max_input_bytes: int_field("max_input_bytes", DEFAULT_MAX_INPUT_BYTES as i64)? as usize,
        max_output_bytes: int_field("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES as i64)? as usize,
        budget_tokens: int_field("budget_tokens", DEFAULT_BUDGET_TOKENS)?,
        apply_to,
        tools,
    })
}

/// Parse da tabela `[savers.<nome>.tools]` (inline ou seção): `compress`
/// obrigatório, `retrieve` opcional, qualquer outra chave é erro.
fn parse_tools(name: &str, spec: &toml::value::Table) -> Result<Option<McpTools>> {
    let Some(value) = spec.get("tools") else {
        return Ok(None);
    };
    let Some(table) = value.as_table() else {
        return Err(Error::Other(format!(
            "savers.toml: savers.{name}.tools deve ser uma tabela \
             (ex.: tools = {{ compress = \"minha_tool\" }})"
        )));
    };
    for key in table.keys() {
        if key != "compress" && key != "retrieve" {
            return Err(Error::Other(format!(
                "savers.toml: savers.{name}.tools.{key} desconhecido (use compress/retrieve)"
            )));
        }
    }
    let compress = table
        .get("compress")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::Other(format!(
                "savers.toml: savers.{name}.tools.compress é obrigatório (string não-vazia)"
            ))
        })?;
    let retrieve = match table.get("retrieve") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::Other(format!(
                        "savers.toml: savers.{name}.tools.retrieve deve ser string não-vazia"
                    ))
                })?
                .to_string(),
        ),
    };
    Ok(Some(McpTools {
        compress: compress.to_string(),
        retrieve,
    }))
}

fn string_array(name: &str, spec: &toml::value::Table, key: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = spec.get(key) else {
        return Ok(None);
    };
    let Some(array) = value.as_array() else {
        return Err(Error::Other(format!(
            "savers.toml: savers.{name}.{key} deve ser uma lista de strings (array argv)"
        )));
    };
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        let Some(s) = entry.as_str() else {
            return Err(Error::Other(format!(
                "savers.toml: savers.{name}.{key} deve conter só strings"
            )));
        };
        out.push(s.to_string());
    }
    Ok(Some(out))
}

/// Template comentado escrito por `ng saver init` (nunca sobrescreve um
/// existente). Os dois exemplos do plano 004: headroom via MCP (contrato
/// 2b) e um binário CLI genérico no contrato 2a.
pub const SAVERS_TOML_TEMPLATE: &str = r#"# not-goldfish — savers externos (compressores plugáveis de token)
#
# TUDO desligado por default. Um saver só roda com enabled = true E depois
# de passar em `ng saver bench <nome>` (status "trusted" — ver `ng saver list`).
#
# PRIVACIDADE: habilitar um saver = enviar tool_output das suas sessões
# àquele processo local. Prompts nunca são enviados (apply_to recusa
# "prompt"), mas tool_output pode conter segredos. Saver que faz rede é
# escolha de quem o instalou — o default OFF garante que é uma escolha.
#
# SEGURANÇA: comandos só podem ser definidos NESTE arquivo global. O
# .ng/config.toml de um projeto (que pode vir de um clone não-confiável)
# só pode ligar/desligar savers daqui por nome — nunca definir command.
#
# Contrato CLI (transport = "cli"):
#   compress:  conteúdo bruto via stdin; stdout = JSON de uma linha
#              {"text": "...", "ref": "opcional"}; exit 0 = sucesso.
#   retrieve:  {"ref": "..."} via stdin; stdout = original cru.
#   "{budget}" no argv é substituído pelo budget em tokens (inteiro).
#   Timeout/saída acima do cap/exit != 0 = falha => pass-through (o
#   conteúdo original segue intacto; nada quebra por causa de um saver).
#
# Contrato MCP (transport = "mcp"):
#   command lança o servidor MCP stdio (JSON-RPC 2.0, um objeto JSON por
#   linha), um processo por chamada, mesmo envelope de segurança do CLI
#   (env limpo, timeout com SIGKILL, caps de bytes).
#   [savers.<nome>.tools] nomeia as tools: compress é obrigatória,
#   retrieve opcional.
#   compress:  tools/call com arguments = {"content": <conteúdo>,
#              "budget_tokens": <inteiro>}; a resposta vem em
#              result.content (blocos {type = "text"}, concatenados). Se o
#              texto for o JSON {"text": "...", "ref": "..."} vale o mesmo
#              contrato do CLI; senão o texto cru inteiro é o digest (sem
#              ref).
#   retrieve:  arguments = {"ref": "..."}; result.content = original cru.
#   result.isError = true, timeout ou saída acima do cap = falha =>
#   pass-through (conteúdo original intacto).
#
# Os tokens economizados são SEMPRE medidos pela heurística do not-goldfish
# (~4 bytes/token) — números auto-reportados pelo saver nunca entram no
# `ng gain`.

# [savers.headroom]
# enabled = false
# transport = "mcp"
# command = ["npx", "-y", "@headroomlabs/headroom-mcp"]   # argv array, NUNCA string shell
# tools = { compress = "headroom_compress", retrieve = "headroom_retrieve" }
# timeout_ms = 2000
# max_input_bytes = 262144   # 256 KiB por item; maior que isso, pass-through
# max_output_bytes = 65536   # saída acima disso = falha do saver
# apply_to = ["tool_output"] # kinds de evento elegíveis; nunca "prompt"

# [savers.meu-compressor]    # exemplo CLI: qualquer binário no contrato acima
# enabled = false
# transport = "cli"
# command = ["my-compressor", "--stdin", "--json", "--budget-tokens", "{budget}"]
# retrieve_command = ["my-compressor", "--retrieve", "--stdin"]
# timeout_ms = 1500
# max_input_bytes = 262144
# max_output_bytes = 65536
# budget_tokens = 256
# apply_to = ["tool_output"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const GLOBAL: &str = r#"
        [savers.meu]
        enabled = true
        transport = "cli"
        command = ["my-compressor", "--budget-tokens", "{budget}"]
        retrieve_command = ["my-compressor", "--retrieve"]
        timeout_ms = 1500

        [savers.headroom]
        enabled = false
        transport = "mcp"
        command = ["npx", "-y", "@headroomlabs/headroom-mcp"]
        tools = { compress = "headroom_compress", retrieve = "headroom_retrieve" }
    "#;

    #[test]
    fn parses_global_file_with_defaults() {
        let config = SaversConfig::from_global_toml(GLOBAL).unwrap();
        assert_eq!(config.savers.len(), 2);
        let meu = config.savers.iter().find(|s| s.name == "meu").unwrap();
        assert!(meu.enabled);
        assert_eq!(meu.transport, Transport::Cli);
        assert_eq!(meu.timeout_ms, 1500);
        assert_eq!(meu.max_input_bytes, 262_144);
        assert_eq!(meu.max_output_bytes, 65_536);
        assert_eq!(meu.budget_tokens, 256);
        assert_eq!(meu.apply_to, vec!["tool_output".to_string()]);
        let hr = config.savers.iter().find(|s| s.name == "headroom").unwrap();
        assert!(!hr.enabled);
        assert_eq!(hr.transport, Transport::Mcp);
        let tools = hr.tools.as_ref().unwrap();
        assert_eq!(tools.compress, "headroom_compress");
        assert_eq!(tools.retrieve.as_deref(), Some("headroom_retrieve"));
        assert!(meu.tools.is_none(), "cli não tem tools");
    }

    #[test]
    fn mcp_without_tools_compress_is_an_error() {
        let err = SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"mcp\"\ncommand = [\"srv\"]\n",
        );
        assert!(
            err.is_err(),
            "mcp sem [savers.x.tools] deve falhar no parse"
        );
        let err = SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"mcp\"\ncommand = [\"srv\"]\ntools = { retrieve = \"r\" }\n",
        );
        assert!(err.is_err(), "tools sem compress deve falhar no parse");
    }

    #[test]
    fn cli_with_tools_is_an_error() {
        let err = SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"cli\"\ncommand = [\"c\"]\ntools = { compress = \"t\" }\n",
        );
        assert!(err.is_err(), "tools em transport cli é erro explícito");
    }

    #[test]
    fn tools_rejects_unknown_keys_and_non_table() {
        assert!(SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"mcp\"\ncommand = [\"srv\"]\n\
             tools = { compress = \"t\", extra = \"y\" }\n",
        )
        .is_err());
        assert!(SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"mcp\"\ncommand = [\"srv\"]\ntools = \"t\"\n",
        )
        .is_err());
    }

    #[test]
    fn empty_or_missing_savers_section_is_no_savers() {
        assert!(SaversConfig::from_global_toml("")
            .unwrap()
            .savers
            .is_empty());
        assert!(SaversConfig::from_global_toml("[outra]\nx = 1\n")
            .unwrap()
            .savers
            .is_empty());
    }

    #[test]
    fn rejects_shell_string_command() {
        let err = SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"cli\"\ncommand = \"sh -c evil\"\n",
        );
        assert!(err.is_err(), "command string (shell) deve ser recusado");
    }

    #[test]
    fn rejects_prompt_in_apply_to() {
        let err = SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"cli\"\ncommand = [\"c\"]\napply_to = [\"prompt\"]\n",
        );
        assert!(err.is_err(), "prompts nunca vão para um saver");
    }

    #[test]
    fn rejects_unknown_transport_and_missing_command() {
        assert!(SaversConfig::from_global_toml(
            "[savers.x]\ntransport = \"http\"\ncommand = [\"c\"]\n"
        )
        .is_err());
        assert!(SaversConfig::from_global_toml("[savers.x]\ntransport = \"cli\"\n").is_err());
    }

    #[test]
    fn template_parses_when_uncommented_sections_absent() {
        // O template é 100% comentado: parseia como "nenhum saver".
        let config = SaversConfig::from_global_toml(SAVERS_TOML_TEMPLATE).unwrap();
        assert!(config.savers.is_empty());
    }

    #[test]
    fn project_toggles_can_enable_and_tune_budget_only() {
        let mut config = SaversConfig::from_global_toml(GLOBAL).unwrap();
        config
            .apply_project_toggles("[savers.headroom]\nenabled = true\nbudget_tokens = 128\n")
            .unwrap();
        let hr = config.savers.iter().find(|s| s.name == "headroom").unwrap();
        assert!(hr.enabled);
        assert_eq!(hr.budget_tokens, 128);
    }

    #[test]
    fn project_file_cannot_define_command() {
        // O teste de segurança central: um repo clonado tentando
        // contrabandear execução de comando é ERRO explícito, não merge.
        let mut config = SaversConfig::from_global_toml(GLOBAL).unwrap();
        let err = config.apply_project_toggles(
            "[savers.meu]\nenabled = true\ncommand = [\"curl\", \"http://evil\"]\n",
        );
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("command"),
            "erro deve citar a chave proibida: {msg}"
        );
        // E a config original segue intacta.
        let meu = config.savers.iter().find(|s| s.name == "meu").unwrap();
        assert_eq!(meu.command[0], "my-compressor");
    }

    #[test]
    fn project_file_cannot_create_new_savers() {
        let mut config = SaversConfig::from_global_toml(GLOBAL).unwrap();
        config
            .apply_project_toggles("[savers.novo]\nenabled = true\n")
            .unwrap();
        assert!(
            !config.savers.iter().any(|s| s.name == "novo"),
            "saver de projeto sem definição global é ignorado, nunca criado"
        );
    }
}
