//! ng-sessions: version-tolerant parsers for AI harness session transcripts,
//! plus a safe rewrite path for procedural hygiene.
//!
//! Every parser reads through `serde_json::Value`, never a rigid struct —
//! harness transcript formats change across releases and we must never hard
//! fail because one vendor added a field. A missing/unrecognized shape
//! downgrades to an `"other"` item with a JSON preview; only an unparsable
//! *line* is dropped (and counted in [`model::Transcript::skipped`]).

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod hygiene;
pub mod kimi;
pub mod model;
pub mod opencode;
pub mod rewrite;

pub use model::{SessionInfo, SessionItem, Transcript};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Scan every known harness's session directory under the user's home and
/// return their metadata, newest first. Directories that don't exist (a
/// harness that was never installed) are skipped, never an error.
pub fn discover_sessions() -> Vec<SessionInfo> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut sessions = Vec::new();
    sessions.extend(claude::discover(&home));
    sessions.extend(codex::discover(&home));
    sessions.extend(gemini::discover(&home));
    sessions.extend(opencode::discover(&home));
    sessions.extend(kimi::discover(&home));
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified_at));
    sessions
}

/// Parse the full transcript for a previously discovered session, dispatching
/// on `info.harness`.
pub fn load_transcript(info: &SessionInfo) -> Result<Transcript> {
    match info.harness.as_str() {
        "claude-code" => claude::parse(info),
        "codex" => codex::parse(info),
        "gemini" => gemini::parse(info),
        "opencode" => opencode::parse(info),
        "kimi" => kimi::parse(info),
        other => Err(Error::Other(format!("unknown harness: {other}"))),
    }
}

/// Best-effort model detection for a session, dispatched on
/// `SessionInfo.harness` — each harness records the model in a different
/// place of its transcript format:
///
/// - **claude-code**: `message.model` em mensagens assistant
///   (`claude-opus-4-7`, ...);
/// - **codex**: `payload.model` do registro `turn_context` do rollout
///   (`gpt-5.4`, ...; `session_meta` só carrega `model_provider`);
/// - **kimi**: `modelAlias` top-level do `config.update` (`kimi-code/k3`),
///   com fallback para `model` top-level (`llm.request`/`usage.record`);
/// - **opencode**: `modelID` top-level dos arquivos de mensagem em
///   `storage/message/<id>/` (o arquivo de sessão em si não registra modelo);
/// - **gemini**: o chat file (`sessionId` + `history`/`messages`) NÃO
///   registra o modelo — retorna `None` honestamente.
///   // TODO(gemini): se uma versão futura do Gemini CLI passar a gravar o
///   // modelo no chat file, adicionar o caminho aqui.
///
/// Returns `None` when the harness doesn't record one or the head of the
/// file doesn't have it — callers must treat it as a hint, never as
/// required data. Bounded read (first ~64 lines / 1 MiB), tolerant to
/// malformed lines: nunca panica, no pior caso retorna `None`.
pub fn detect_model(info: &SessionInfo) -> Option<String> {
    match info.harness.as_str() {
        "claude-code" => detect_in_lines(&info.path, |v| {
            v.get("message")
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        }),
        "codex" => detect_in_lines(&info.path, |v| {
            v.get("payload")
                .and_then(|p| p.get("model"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        }),
        "kimi" => detect_in_lines(&info.path, |v| {
            v.get("modelAlias")
                .and_then(|m| m.as_str())
                .or_else(|| v.get("model").and_then(|m| m.as_str()))
                .map(str::to_string)
        }),
        "opencode" => detect_opencode_model(info),
        // gemini: o chat file não registra o modelo (ver docstring acima).
        _ => None,
    }
}

/// Scan the head of a JSONL transcript (first ~64 lines / 1 MiB) applying
/// `extract` to each parsed line; returns the first model string found.
fn detect_in_lines(
    path: &std::path::Path,
    extract: impl Fn(&serde_json::Value) -> Option<String>,
) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut total = 0usize;
    for _ in 0..64 {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        total += n;
        if total > (1 << 20) {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if let Some(model) = extract(&value) {
            return Some(model);
        }
    }
    None
}

/// opencode's session file (`storage/session/<ws>/<id>.json`) carries no
/// model — the model lives in the per-message files under
/// `storage/message/<id>/` as a top-level `modelID` string (`gpt-4.1`,
/// `claude-sonnet-4-...`). Reads the first few message files (filename
/// order) and returns the first `modelID` found; `None` when the message
/// directory is absent or no file records it.
fn detect_opencode_model(info: &SessionInfo) -> Option<String> {
    let storage_root = info
        .path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())?;
    let mut message_files: Vec<_> = std::fs::read_dir(storage_root.join("message").join(&info.id))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    message_files.sort();
    for path in message_files.into_iter().take(8) {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if let Some(model) = value.get("modelID").and_then(|m| m.as_str()) {
            return Some(model.to_string());
        }
    }
    None
}

/// First `PREVIEW_CHARS` chars of `text`, safe on non-ASCII (never splits a
/// UTF-8 codepoint).
pub(crate) fn preview(text: &str) -> String {
    match text.char_indices().nth(model::PREVIEW_CHARS) {
        Some((cut, _)) => text[..cut].to_string(),
        None => text.to_string(),
    }
}

/// Rough token estimate: ~4 bytes per token, the same heuristic ng-core uses.
pub(crate) fn tokens_est(text: &str) -> i64 {
    (text.len() / 4) as i64
}

/// Extract flattened text and a shape label from a `message.content` value
/// shared by the Claude-style and Kimi wire formats: either a plain string,
/// or an array of typed blocks (`text`, `tool_use`/`function_call`,
/// `tool_result`/`function_call_output`, or anything else it doesn't know,
/// which contributes nothing to the text but is still counted in `kind`).
pub(crate) fn extract_content_text(content: &serde_json::Value) -> (String, String) {
    use serde_json::Value;
    match content {
        Value::String(s) => (s.clone(), "text".to_string()),
        Value::Array(blocks) => {
            let mut text = String::new();
            let mut kinds: Vec<String> = Vec::new();
            let push = |text: &mut String, piece: &str| {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(piece);
            };
            for block in blocks {
                let btype = block
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("other");
                match btype {
                    "text" | "input_text" | "output_text" => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            push(&mut text, t);
                        }
                        kinds.push("text".to_string());
                    }
                    "tool_use" | "function_call" => {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        push(&mut text, &format!("[tool_use: {name}]"));
                        kinds.push("tool_use".to_string());
                    }
                    "tool_result" | "function_call_output" => {
                        let inner = block.get("content").unwrap_or(block);
                        let (inner_text, _) = extract_content_text(inner);
                        if !inner_text.is_empty() {
                            push(&mut text, &inner_text);
                        } else if let Some(s) = block.get("output").and_then(|o| o.as_str()) {
                            push(&mut text, s);
                        }
                        kinds.push("tool_result".to_string());
                    }
                    other => kinds.push(other.to_string()),
                }
            }
            let kind = match kinds.split_first() {
                Some((first, rest)) if rest.iter().all(|k| k == first) => first.clone(),
                Some(_) => "mixed".to_string(),
                None => "other".to_string(),
            };
            (text, kind)
        }
        _ => (String::new(), "other".to_string()),
    }
}

/// Cheap items hint from file size, used during discovery where reading
/// every session fully would be too expensive: ~200 bytes/line is a
/// reasonable average for these transcript formats.
pub(crate) fn size_hint(path: &std::path::Path) -> Option<usize> {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.len() as usize / 200).max(1))
}

/// Read `value.timestamp` (a top-level RFC3339 string, the shape both
/// Claude Code and Codex use per-line) and parse it into a Unix epoch.
/// `None` when the field is missing or malformed — never an error, since a
/// timestamp is an enrichment, not something a parser can require.
pub(crate) fn item_timestamp(value: &serde_json::Value) -> Option<i64> {
    value
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_rfc3339_epoch)
}

/// Parse an RFC3339 timestamp (`2026-07-18T10:00:00Z`, optionally with
/// fractional seconds and/or a numeric `+HH:MM`/`-HH:MM` offset instead of
/// `Z`) into a Unix epoch in seconds. Returns `None` on anything that
/// doesn't match rather than failing the caller — a malformed or
/// unexpected timestamp shape must never abort a parse. `days_from_civil`
/// below is the inverse of `civil_from_days`, which `ng-hook`/`ng-cli`
/// already use for the same Gregorian date math in the other direction; it
/// is re-derived here rather than shared because `ng-sessions` has no
/// dependency on those crates.
pub(crate) fn parse_rfc3339_epoch(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    let year: i64 = s.get(0..4)?.parse().ok()?;
    if bytes.get(4) != Some(&b'-') {
        return None;
    }
    let month: i64 = s.get(5..7)?.parse().ok()?;
    if bytes.get(7) != Some(&b'-') {
        return None;
    }
    let day: i64 = s.get(8..10)?.parse().ok()?;
    if !matches!(bytes.get(10), Some(b'T') | Some(b't') | Some(b' ')) {
        return None;
    }
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    if bytes.get(13) != Some(&b':') {
        return None;
    }
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    if bytes.get(16) != Some(&b':') {
        return None;
    }
    let second: i64 = s.get(17..19)?.parse().ok()?;

    // Everything after the seconds field is optional fractional seconds
    // (".123") followed by either "Z"/"z" or a "+HH:MM"/"-HH:MM" offset.
    // Only whole-second precision matters here, so fractional digits are
    // skipped rather than parsed.
    let rest = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset_minutes = if rest.is_empty() || rest.eq_ignore_ascii_case("z") {
        0
    } else if let Some(off) = rest.strip_prefix('+') {
        parse_offset_minutes(off)?
    } else {
        let off = rest.strip_prefix('-')?;
        -parse_offset_minutes(off)?
    };

    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60)
}

/// Parse a `HH:MM` or `HHMM` UTC offset magnitude into minutes.
fn parse_offset_minutes(s: &str) -> Option<i64> {
    let (h, m) = if let Some((h, m)) = s.split_once(':') {
        (h, m)
    } else if s.len() == 4 {
        (&s[0..2], &s[2..4])
    } else {
        return None;
    };
    Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?)
}

/// Days since the Unix epoch for a Gregorian calendar date. Howard
/// Hinnant's `days_from_civil` algorithm — the inverse of `civil_from_days`
/// used elsewhere in this workspace for the same date math.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod detect_model_tests {
    use super::*;
    use std::io::Write;

    fn info(harness: &str, path: std::path::PathBuf) -> SessionInfo {
        SessionInfo {
            id: "s1".into(),
            harness: harness.into(),
            items_hint: None,
            path,
            project: None,
            modified_at: std::time::SystemTime::now(),
        }
    }

    fn write_jsonl(dir: &std::path::Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn claude_reads_message_model() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "s.jsonl",
            &[
                r#"{"type":"user","message":{"role":"user","content":"oi"}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-7","content":[]}}"#,
            ],
        );
        assert_eq!(
            detect_model(&info("claude-code", path)),
            Some("claude-opus-4-7".to_string())
        );
    }

    #[test]
    fn codex_reads_turn_context_payload_model() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "rollout-s1.jsonl",
            &[
                r#"{"timestamp":"2026-04-16T09:08:50Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                r#"{"timestamp":"2026-04-16T09:08:51Z","type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#,
                r#"{"timestamp":"2026-04-16T09:08:52Z","type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5.4"}}"#,
            ],
        );
        assert_eq!(
            detect_model(&info("codex", path)),
            Some("gpt-5.4".to_string())
        );
    }

    #[test]
    fn codex_session_meta_sem_model_retorna_none() {
        let tmp = tempfile::tempdir().unwrap();
        // session_meta só carrega model_provider, nunca o nome do modelo.
        let path = write_jsonl(
            tmp.path(),
            "rollout-s1.jsonl",
            &[
                r#"{"type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[]}}"#,
            ],
        );
        assert_eq!(detect_model(&info("codex", path)), None);
    }

    #[test]
    fn kimi_reads_model_alias_do_config_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "wire.jsonl",
            &[
                r#"{"type":"metadata","protocol_version":"1.4"}"#,
                r#"{"type":"config.update","modelAlias":"kimi-code/k3","time":1784623500000}"#,
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"oi"}],"time":1784623501000}"#,
            ],
        );
        assert_eq!(
            detect_model(&info("kimi", path)),
            Some("kimi-code/k3".to_string())
        );
    }

    #[test]
    fn kimi_cai_no_model_top_level_sem_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "wire.jsonl",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"oi"}],"time":1784623501000}"#,
                r#"{"type":"llm.request","model":"k3","time":1784623502000}"#,
            ],
        );
        assert_eq!(detect_model(&info("kimi", path)), Some("k3".to_string()));
    }

    #[test]
    fn opencode_le_model_id_da_pasta_de_mensagens() {
        let tmp = tempfile::tempdir().unwrap();
        // storage/session/<ws>/s1.json + storage/message/s1/msg_*.json
        let session_dir = tmp.path().join("storage").join("session").join("ws");
        std::fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("s1.json");
        std::fs::write(&session_path, r#"{"id":"s1","title":"sessão"}"#).unwrap();
        let msg_dir = tmp.path().join("storage").join("message").join("s1");
        std::fs::create_dir_all(&msg_dir).unwrap();
        std::fs::write(
            msg_dir.join("msg_001.json"),
            r#"{"id":"msg_001","role":"user","content":"oi"}"#,
        )
        .unwrap();
        std::fs::write(
            msg_dir.join("msg_002.json"),
            r#"{"id":"msg_002","role":"assistant","modelID":"gpt-4.1","providerID":"openai","content":[]}"#,
        )
        .unwrap();
        assert_eq!(
            detect_model(&info("opencode", session_path)),
            Some("gpt-4.1".to_string())
        );
    }

    #[test]
    fn opencode_sem_pasta_de_mensagens_retorna_none() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("storage").join("session").join("ws");
        std::fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("s1.json");
        std::fs::write(&session_path, r#"{"id":"s1","title":"sessão"}"#).unwrap();
        assert_eq!(detect_model(&info("opencode", session_path)), None);
    }

    #[test]
    fn gemini_retorna_none_chat_file_nao_registra_modelo() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session-s1.json");
        std::fs::write(
            &path,
            r#"{"sessionId":"s1","history":[{"role":"user","parts":[{"text":"oi"}]},{"role":"model","parts":[{"text":"olá"}]}]}"#,
        )
        .unwrap();
        assert_eq!(detect_model(&info("gemini", path)), None);
    }

    #[test]
    fn transcript_malformado_nunca_panica_retorna_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "s.jsonl",
            &["not json at all", r#"{"type":"user"#, "", r#"{"foo":42}"#],
        );
        assert_eq!(detect_model(&info("claude-code", path.clone())), None);
        assert_eq!(detect_model(&info("codex", path.clone())), None);
        assert_eq!(detect_model(&info("kimi", path)), None);
    }

    #[test]
    fn arquivo_inexistente_retorna_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nao-existe.jsonl");
        assert_eq!(detect_model(&info("claude-code", path)), None);
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    #[test]
    fn parses_z_suffix() {
        assert_eq!(
            parse_rfc3339_epoch("2026-07-18T10:00:00Z"),
            Some(1_784_368_800)
        );
    }

    #[test]
    fn parses_positive_offset() {
        assert_eq!(
            parse_rfc3339_epoch("2026-07-18T10:00:00+02:00"),
            Some(1_784_361_600)
        );
    }

    #[test]
    fn parses_fractional_seconds() {
        assert_eq!(
            parse_rfc3339_epoch("2026-07-18T10:00:00.123Z"),
            Some(1_784_368_800)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_rfc3339_epoch("not a timestamp"), None);
        assert_eq!(parse_rfc3339_epoch(""), None);
    }

    #[test]
    fn item_timestamp_reads_top_level_field() {
        let value =
            serde_json::json!({"timestamp": "2026-07-18T10:00:00Z", "type": "response_item"});
        assert_eq!(item_timestamp(&value), Some(1_784_368_800));
    }

    #[test]
    fn item_timestamp_missing_field_is_none() {
        let value = serde_json::json!({"type": "response_item"});
        assert_eq!(item_timestamp(&value), None);
    }
}
