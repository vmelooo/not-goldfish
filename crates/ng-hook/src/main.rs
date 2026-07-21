//! ng-hook: ultra-thin binary invoked by harness hooks on every message.
//!
//! Hot-path budget is <5ms, so this binary does the minimum: parse the hook
//! payload from stdin, extract lexical tags, and hand the event to the
//! daemon over a unix socket. If the daemon is down it degrades to a direct
//! SQLite write — capture must never depend on the daemon being alive.
//!
//! It always exits 0: a capture failure must never break the user's session.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ng_core::{lex, paths, Event, GainEnvelope, GainRecord, Store};
use ng_hook::{inject, precompact};

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }

    // PreCompact hygiene runs independently of (and before) normal capture
    // below: it's opt-in via NG_AUTO_HYGIENE, and its own failures are
    // already swallowed inside handle_precompact, so a bad transcript here
    // can never stop the event from also being captured normally.
    if let Some(outcome) = maybe_run_precompact(&input) {
        println!("{}", outcome.response);
        // Ledger da higiene, só depois do rename atômico (que já aconteceu
        // dentro de handle_precompact). PreCompact é raro — o fallback de
        // escrita direta quando o daemon está fora é aceitável aqui, ao
        // contrário do hot path de prompt logo abaixo.
        send_gain(outcome.gain, true);
    }

    let Some(event) = parse_hook_payload(&input) else {
        return;
    };
    let timing = std::env::var("NG_DEBUG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    if send_to_daemon(&event).is_err() {
        // Self-healing: bring the daemon up for the NEXT events (this one
        // still goes through the direct-write fallback below). ngd itself
        // serializes concurrent spawns with an flock, so a storm of hooks
        // costs at most a few no-op forks.
        if !matches!(std::env::var("NG_AUTOSTART").as_deref(), Ok("0")) {
            spawn_daemon();
        }
        // Fallback: direct write. WAL mode + busy_timeout make concurrent
        // writers from parallel sessions safe. `open_bounded` (não `open`):
        // o retry longo do open comum foi calibrado para o cold start do
        // daemon e bloquearia até ~3s aqui no hot path — melhor perder esta
        // captura do que segurar o prompt do usuário.
        if timing {
            eprintln!("daemon_try: {:?}", t0.elapsed());
        }
        let t1 = std::time::Instant::now();
        if let Ok(store) = Store::open_bounded(&paths::db_path()) {
            if timing {
                eprintln!("store_open: {:?}", t1.elapsed());
            }
            let t2 = std::time::Instant::now();
            let _ = store.insert_event(&event);
            if timing {
                eprintln!("insert: {:?}", t2.elapsed());
            }
        }
    }

    // Proactive memory: on user prompts, surface relevant past-session
    // memories as additionalContext. [finding 01] Read-only open, never RW
    // — see `inject::build_injection_readonly`'s doc comment for why the
    // schema-init/retry machinery must never run on this hot path.
    if event.kind == "prompt" {
        if let Some(injection) =
            inject::build_injection_readonly(&paths::db_path(), &event.content, &event.session_id)
        {
            // Custo declarado da injeção, para o gain_ledger. Registrado
            // antes de mover o contexto para a resposta; entregue só via
            // socket (sem fallback RW) — no hot path de prompt uma métrica
            // perdida é aceitável, um open read-write nunca (ver o doc de
            // build_injection_readonly).
            let gain = GainRecord {
                kind: "inject".to_string(),
                session_id: event.session_id.clone(),
                project: event.project.clone(),
                tokens: injection.tokens_est,
                items: injection.items,
                created_at: event.created_at,
            };
            println!("{}", inject::hook_response(injection.context));
            send_gain(gain, false);
        }
    }
}

/// Best-effort delivery of one gain-ledger record. Metric only: every
/// failure is swallowed and the exit code stays 0 — nunca quebra a sessão.
/// Never spawns the daemon. With `direct_fallback` (cold paths only, e.g.
/// PreCompact) a daemon miss degrades to one direct SQLite write; on the
/// per-prompt hot path it must stay `false` so the ledger can never put a
/// write-capable `Store::open` back on the <5ms budget.
fn send_gain(record: GainRecord, direct_fallback: bool) {
    let envelope = GainEnvelope { ng_gain: record };
    let Ok(mut line) = serde_json::to_vec(&envelope) else {
        return;
    };
    line.push(b'\n');
    let sent = UnixStream::connect(paths::socket_path())
        .and_then(|mut stream| {
            stream.set_write_timeout(Some(Duration::from_millis(50)))?;
            stream.write_all(&line)
        })
        .is_ok();
    if !sent && direct_fallback {
        if let Ok(store) = Store::open(&paths::db_path()) {
            let _ = store.insert_gain(&envelope.ng_gain);
        }
    }
}

/// Map a Claude Code-style hook payload (also emitted by Kimi Code, which
/// clones the schema) into an [`Event`]. Returns `None` for payloads that
/// carry nothing worth remembering.
fn parse_hook_payload(input: &str) -> Option<Event> {
    let payload: serde_json::Value = serde_json::from_str(input).ok()?;
    let hook = payload.get("hook_event_name")?.as_str()?;

    let (kind, content, meta) = match hook {
        "UserPromptSubmit" => ("prompt", payload.get("prompt")?.as_str()?.to_string(), None),
        "PostToolUse" => {
            let tool = payload
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let tool_input = payload.get("tool_input");
            let input_str = tool_input
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            let response = payload
                .get("tool_response")
                .map(compact_value)
                .unwrap_or_default();

            let mut meta_obj = serde_json::json!({ "tool": tool });
            if let Some(path) = tool_input.and_then(extract_file_path) {
                meta_obj["file_path"] = serde_json::Value::String(path);
            }

            let command = tool_input
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let kind = if is_ng_command(command) || has_ng_markers(&response) {
                "ng_meta"
            } else {
                "tool_output"
            };
            (
                kind,
                format!("[{tool}] input: {input_str}\noutput: {response}"),
                Some(meta_obj.to_string()),
            )
        }
        "SessionStart" => ("session_start", String::new(), None),
        "Stop" | "SessionEnd" => {
            let meta = payload
                .get("transcript_path")
                .and_then(|v| v.as_str())
                .map(|p| serde_json::json!({ "transcript_path": p }).to_string());
            ("session_end", String::new(), meta)
        }
        // Captured as a plain marker event like SessionStart/Stop; the
        // actual hygiene work (if enabled) happens in maybe_run_precompact
        // before parse_hook_payload is even called.
        "PreCompact" => ("precompact", String::new(), None),
        _ => return None,
    };

    let event = Event {
        session_id: payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        project: payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        harness: std::env::var("NG_HARNESS").unwrap_or_else(|_| "claude-code".into()),
        kind: kind.to_string(),
        tags: lex::extract_tags(&content),
        content,
        meta,
        created_at: now_epoch(),
    }
    .cap_content();
    Some(event)
}

/// Structured path field of the common file tools (Read/Edit/Write/
/// NotebookEdit). Only absolute-ish paths count — tool inputs carry other
/// string fields we must not mistake for paths.
fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(path) = tool_input.get(key).and_then(|v| v.as_str()) {
            if path.contains('/') {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// True when some command in the line invokes the `ng` binary itself:
/// split on shell connectors, take each segment's first token, strip any
/// path prefix, compare with "ng" exactly.
fn is_ng_command(command: &str) -> bool {
    command
        .split([';', '|', '&'])
        .filter_map(|segment| segment.split_whitespace().next())
        .any(|first| {
            let bin = first.rsplit('/').next().unwrap_or(first);
            bin == "ng"
        })
}

/// Output that visibly came from not-goldfish itself (stubs, injected
/// memory blocks) — re-capturing it as ordinary tool_output would feed the
/// memory back into itself.
fn has_ng_markers(output: &str) -> bool {
    output.contains("[ng-evicted:") || output.contains("<not-goldfish-memory>")
}

/// Cheap pre-check for `PreCompact` plus the `NG_AUTO_HYGIENE` gate, kept
/// separate from `precompact::handle_precompact` so the env read (a real
/// side effect) happens exactly once, here, in the one place that isn't
/// unit tested against a fake environment.
fn maybe_run_precompact(input: &str) -> Option<precompact::PrecompactOutcome> {
    let payload: serde_json::Value = serde_json::from_str(input).ok()?;
    if payload.get("hook_event_name")?.as_str()? != "PreCompact" {
        return None;
    }
    let force_enabled = matches!(std::env::var("NG_AUTO_HYGIENE").as_deref(), Ok("1"));
    precompact::handle_precompact(input, force_enabled)
}

/// Tool responses are often JSON envelopes; extract the text if it is the
/// common `{"output": "..."}`-ish shape, otherwise serialize as-is.
fn compact_value(value: &serde_json::Value) -> String {
    for key in ["output", "stdout", "content", "result", "text"] {
        if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
            return text.to_string();
        }
    }
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    serde_json::to_string(value).unwrap_or_default()
}

/// Best-effort detached spawn of the sibling `ngd` binary.
fn spawn_daemon() {
    let Some(ngd) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ngd")))
        .filter(|path| path.exists())
    else {
        return;
    };
    let _ = std::process::Command::new(ngd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn send_to_daemon(event: &Event) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(paths::socket_path())?;
    stream.set_write_timeout(Some(Duration::from_millis(50)))?;
    let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_tool_payload(tool: &str, input: serde_json::Value, response: &str) -> String {
        serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": "s1",
            "cwd": "/tmp/proj",
            "tool_name": tool,
            "tool_input": input,
            "tool_response": {"output": response},
        })
        .to_string()
    }

    #[test]
    fn post_tool_use_carries_structured_meta_with_file_path() {
        let payload = post_tool_payload(
            "Read",
            serde_json::json!({"file_path": "/tmp/proj/src/lib.rs"}),
            "conteudo",
        );
        let event = parse_hook_payload(&payload).unwrap();
        assert_eq!(event.kind, "tool_output");
        let meta: serde_json::Value = serde_json::from_str(event.meta.as_deref().unwrap()).unwrap();
        assert_eq!(meta["tool"], "Read");
        assert_eq!(meta["file_path"], "/tmp/proj/src/lib.rs");
    }

    #[test]
    fn post_tool_use_without_path_has_tool_only_meta() {
        let payload = post_tool_payload("Bash", serde_json::json!({"command": "ls -la"}), "ok");
        let event = parse_hook_payload(&payload).unwrap();
        let meta: serde_json::Value = serde_json::from_str(event.meta.as_deref().unwrap()).unwrap();
        assert_eq!(meta["tool"], "Bash");
        assert!(meta.get("file_path").is_none());
    }

    #[test]
    fn ng_command_output_is_kind_ng_meta() {
        let payload = post_tool_payload(
            "Bash",
            serde_json::json!({"command": "ng search --here consulta"}),
            "resultados…",
        );
        assert_eq!(parse_hook_payload(&payload).unwrap().kind, "ng_meta");
    }

    #[test]
    fn ng_marker_in_output_is_kind_ng_meta() {
        let payload = post_tool_payload(
            "Read",
            serde_json::json!({"file_path": "/tmp/t.jsonl"}),
            "linha com [ng-evicted: abc — recupere com `ng search`]",
        );
        assert_eq!(parse_hook_payload(&payload).unwrap().kind, "ng_meta");
    }

    #[test]
    fn non_ng_bash_command_stays_tool_output() {
        let payload = post_tool_payload(
            "Bash",
            serde_json::json!({"command": "cargo test --workspace"}),
            "ok",
        );
        assert_eq!(parse_hook_payload(&payload).unwrap().kind, "tool_output");
    }

    #[test]
    fn stop_carries_transcript_path_meta() {
        let payload = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "s1",
            "cwd": "/tmp/proj",
            "transcript_path": "/home/u/.claude/projects/x/s1.jsonl",
        })
        .to_string();
        let event = parse_hook_payload(&payload).unwrap();
        assert_eq!(event.kind, "session_end");
        let meta: serde_json::Value = serde_json::from_str(event.meta.as_deref().unwrap()).unwrap();
        assert_eq!(
            meta["transcript_path"],
            "/home/u/.claude/projects/x/s1.jsonl"
        );
    }

    #[test]
    fn is_ng_command_matches_only_real_invocations() {
        assert!(is_ng_command("ng search foo"));
        assert!(is_ng_command("./target/release/ng status"));
        assert!(is_ng_command("cd /x && ng wisdom --md"));
        assert!(!is_ng_command("npm run ng-build"));
        assert!(!is_ng_command("running total"));
        // Primeiro token do segmento é "angular", não "ng":
        assert!(!is_ng_command("angular ng serve"));
    }
}
