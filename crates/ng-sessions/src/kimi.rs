//! Kimi transcripts: wire format is a flat JSONL event log — no `message`
//! wrapper like Claude Code, so each line is read directly as
//! `{ role | type, content }`.
//!
//! Layouts conhecidos (ambos suportados pelo discovery):
//! - atual (Kimi Code CLI): `~/.kimi-code/sessions/<workspace>/<id>/agents/<agent>/wire.jsonl`
//! - legado: `~/.kimi/sessions/<workspace>/<id>/wire.jsonl`

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::model::{SessionInfo, SessionItem, Transcript};
use crate::{extract_content_text, preview, size_hint, tokens_est, Result};

pub const HARNESS: &str = "kimi";

/// Coleta `wire.jsonl` nos dois formatos de diretório abaixo de `root`
/// (que já inclui `sessions/`): `<ws>/<session>/wire.jsonl` (legado) e
/// `<ws>/<session>/agents/<agent>/wire.jsonl` (atual — uma sessão pode ter
/// um wire por agente; cada um vira uma entrada própria).
fn discover_in(root: &Path, out: &mut Vec<SessionInfo>) {
    let Ok(workspace_dirs) = fs::read_dir(root) else {
        return;
    };
    for workspace_dir in workspace_dirs.flatten() {
        let workspace_path = workspace_dir.path();
        if !workspace_path.is_dir() {
            continue;
        }
        let project = workspace_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        let Ok(session_dirs) = fs::read_dir(&workspace_path) else {
            continue;
        };
        for session_dir in session_dirs.flatten() {
            let session_path = session_dir.path();
            let Some(session_id) = session_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
            else {
                continue;
            };
            // legado: wire.jsonl direto na pasta da sessão
            push_wire(&session_path.join("wire.jsonl"), &session_id, &project, out);
            // atual: um wire por agente
            let agents_dir = session_path.join("agents");
            if let Ok(agent_dirs) = fs::read_dir(&agents_dir) {
                for agent_dir in agent_dirs.flatten() {
                    let agent_path = agent_dir.path();
                    let agent = agent_path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "agent".to_string());
                    push_wire(
                        &agent_path.join("wire.jsonl"),
                        &format!("{session_id}/{agent}"),
                        &project,
                        out,
                    );
                }
            }
        }
    }
}

fn push_wire(wire: &Path, id: &str, project: &Option<String>, out: &mut Vec<SessionInfo>) {
    if !wire.is_file() {
        return;
    }
    let modified_at = fs::metadata(wire)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH);
    out.push(SessionInfo {
        id: id.to_string(),
        harness: HARNESS.to_string(),
        items_hint: size_hint(wire),
        path: wire.to_path_buf(),
        project: project.clone(),
        modified_at,
    });
}

/// Find every `wire.jsonl` under `~/.kimi-code/sessions` (layout atual) e
/// `~/.kimi/sessions` (legado).
pub fn discover(home: &Path) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for root in [
        home.join(".kimi-code").join("sessions"),
        home.join(".kimi").join("sessions"),
    ] {
        discover_in(&root, &mut out);
    }
    out
}

pub fn parse(info: &SessionInfo) -> Result<Transcript> {
    let content = fs::read_to_string(&info.path)?;
    let mut items = Vec::new();
    let mut skipped = 0usize;

    for (line_no, line) in content.lines().enumerate() {
        let raw_line = line_no + 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if let Some(item) = item_from_value(items.len(), raw_line, &value) {
            items.push(item);
        }
    }

    Ok(Transcript {
        info: info.clone(),
        items,
        skipped,
    })
}

/// Tipos de linha do wire que são telemetria/meta — não viram itens de
/// transcript (barulho puro para leitura). Silenciosamente ignoradas, como
/// linhas vazias: o `skipped` continua contando só JSON inválido e o
/// `raw_line` continua físico. `config.update` é meta para a UI, mas segue
/// sendo lida por `detect_model` (que varre as linhas cruas).
fn is_meta_type(ty: &str) -> bool {
    matches!(
        ty,
        "metadata"
            | "config.update"
            | "llm.request"
            | "llm.tools_snapshot"
            | "usage.record"
            | "goal.update"
            | "goal.create"
            | "goal.clear"
            | "tools.update_store"
            | "tools.set_active_tools"
            | "permission.set_mode"
            | "permission.record_approval_result"
            | "plan_mode.enter"
            | "plan_mode.cancel"
            | "turn.steer"
            | "turn.cancel"
            | "step.begin"
            | "step.end"
    )
}

/// Converte uma linha do wire em item de transcript, ou `None` para linhas
/// meta. O wire atual (Kimi Code CLI) embrulha a maioria dos eventos em
/// `context.append_loop_event` — desembrulhamos; formas antigas/flat
/// (`{role|type, content}`) seguem cobertas pelo fallback.
fn item_from_value(index: usize, raw_line: usize, value: &Value) -> Option<SessionItem> {
    let timestamp = value
        .get("time")
        .and_then(|t| t.as_i64())
        .map(|ms| ms / 1000);
    let ty = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if is_meta_type(ty) {
        return None;
    }

    // desembrulha o envelope do wire atual
    let (ty, inner) = if ty == "context.append_loop_event" {
        let e = value.get("event")?;
        (e.get("type").and_then(|t| t.as_str()).unwrap_or(""), e)
    } else {
        (ty, value)
    };
    if is_meta_type(ty) {
        return None;
    }

    let (role, kind, text) = match ty {
        "turn.prompt" => {
            let (text, _) = inner
                .get("input")
                .map(extract_content_text)
                .unwrap_or_default();
            ("user".to_string(), "prompt".to_string(), text)
        }
        "context.append_message" => {
            let msg = inner.get("message")?;
            let role = match msg.get("role").and_then(|r| r.as_str()) {
                Some("user") => "user",
                Some("assistant") => "assistant",
                Some("system") => "system",
                Some("tool") => "tool",
                _ => "other",
            }
            .to_string();
            let (text, kind) = msg
                .get("content")
                .map(extract_content_text)
                .unwrap_or_default();
            (role, kind, text)
        }
        "content.part" => {
            let part = inner.get("part")?;
            let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("text");
            let kind = if ptype == "think" { "thinking" } else { ptype };
            let text = part
                .get("text")
                .or_else(|| part.get("think"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            ("assistant".to_string(), kind.to_string(), text)
        }
        "tool.call" => {
            let name = inner.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let input = inner
                .get("input")
                .map(|i| {
                    let s = i.to_string();
                    s.chars().take(200).collect::<String>()
                })
                .unwrap_or_default();
            (
                "assistant".to_string(),
                "tool_use".to_string(),
                format!("[tool_use: {name}] {input}"),
            )
        }
        "tool.result" => {
            let out = inner
                .get("result")
                .and_then(|r| r.get("output"))
                .and_then(|o| o.as_str())
                .map(|s| s.to_string())
                .or_else(|| inner.get("result").map(|r| r.to_string()))
                .unwrap_or_default();
            ("tool".to_string(), "tool_result".to_string(), out)
        }
        // fallback flat/legado: {role|type, content}
        _ => {
            let role = inner
                .get("role")
                .and_then(|r| r.as_str())
                .or(Some(ty))
                .map(|r| match r {
                    "user" | "assistant" | "system" | "tool" => r.to_string(),
                    _ => "other".to_string(),
                })
                .unwrap_or_else(|| "other".to_string());
            let (text, kind) = match inner.get("content") {
                Some(content) => extract_content_text(content),
                None => (String::new(), "other".to_string()),
            };
            (role, kind, text)
        }
    };

    let text_full = if text.is_empty() {
        inner.to_string()
    } else {
        text
    };

    Some(SessionItem {
        index,
        role,
        kind,
        tokens_est: tokens_est(&text_full),
        text_preview: preview(&text_full),
        text_full,
        timestamp,
        raw_line: Some(raw_line),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(discover(tmp.path()).is_empty());
    }

    #[test]
    fn wire_layout_unwraps_loop_events_and_skips_meta() {
        let dir = tempfile::tempdir().unwrap();
        let wire = dir.path().join("wire.jsonl");
        let lines = [
            r#"{"type":"metadata","protocol_version":"1.4"}"#,
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"o que faz o ng gain?"}],"time":1784623500000}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"analisando..."}},"time":1784623501000}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"mostra o ganho acumulado"}},"time":1784623502000}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","name":"Bash","input":{"command":"ng gain"}},"time":1784623503000}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","result":{"output":"12 431 eventos"}},"time":1784623504000}"#,
            r#"{"type":"usage.record","x":1}"#,
            r#"{"type":"context.append_loop_event","event":{"type":"step.begin","step":1},"time":1784623505000}"#,
        ];
        std::fs::write(&wire, lines.join("\n") + "\n").unwrap();
        let info = SessionInfo {
            id: "s".into(),
            harness: HARNESS.into(),
            path: wire,
            project: None,
            modified_at: std::time::SystemTime::now(),
            items_hint: None,
        };
        let t = parse(&info).unwrap();
        let roles: Vec<&str> = t.items.iter().map(|i| i.role.as_str()).collect();
        assert_eq!(
            roles,
            ["user", "assistant", "assistant", "assistant", "tool"]
        );
        assert_eq!(t.items[0].kind, "prompt");
        assert_eq!(t.items[1].kind, "thinking");
        assert_eq!(t.items[2].kind, "text");
        assert_eq!(t.items[3].kind, "tool_use");
        assert_eq!(t.items[4].kind, "tool_result");
        assert_eq!(t.items[4].text_preview, "12 431 eventos");
        assert_eq!(t.items[0].timestamp, Some(1784623500));
        assert_eq!(t.skipped, 0, "linhas meta não contam como skipped");
    }
}
