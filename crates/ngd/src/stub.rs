//! JSONL stub/rewrite engine backing the UI's `/api/rewrite` endpoint.
//!
//! Pure, synchronous helpers extracted from `ui.rs` (ARCH-06): they turn
//! 1-based line numbers into `(line, replacement)` pairs where the target
//! line's content field is swapped for a recoverable `[ng-evicted: ...]`
//! stub, leaving every other field byte-identical.
//!
//! [finding 19b] Line-targeting invariant: everything here MUST split lines
//! with the canonical [`ng_sessions::rewrite::split_lines`] — the same
//! splitter [`ng_sessions::rewrite::rewrite_jsonl`] uses — so this module
//! and the rewriter can never disagree about which physical line a 1-based
//! number addresses (CRLF transcripts are where `str::lines()` and
//! `split_lines` diverge). Keep any future line math on `split_lines`.

use ng_sessions::rewrite::split_lines;

/// How much of the original content survives (as a preview) inside a stub,
/// so an evicted item is still recognizable at a glance.
const STUB_PREVIEW_CHARS: usize = 80;

/// JSON pointers (as key paths) where each harness keeps a line's message
/// content, tried in order until one exists on the parsed line. Mirrors the
/// shapes `ng_sessions::{claude,codex,kimi}::item_from_value` already read
/// — inclusive o wire atual do kimi (`context.append_loop_event` com
/// `event.part`/`event.result.output` e `turn.prompt.input`).
fn content_pointers(harness: &str) -> Vec<Vec<&'static str>> {
    match harness {
        "claude-code" => vec![vec!["message", "content"]],
        "kimi" => vec![
            vec!["content"],                   // flat legado
            vec!["message", "content"],        // context.append_message
            vec!["input"],                     // turn.prompt (array de blocos)
            vec!["event", "part", "text"],     // content.part (texto)
            vec!["event", "part", "think"],    // content.part (thinking)
            vec!["event", "result", "output"], // tool.result
        ],
        "codex" => vec![vec!["payload", "content"], vec!["payload", "output"]],
        _ => Vec::new(),
    }
}

fn walk<'v>(value: &'v serde_json::Value, pointer: &[&str]) -> Option<&'v serde_json::Value> {
    let mut current = value;
    for key in pointer {
        current = current.get(key)?;
    }
    Some(current)
}

fn walk_mut<'v>(
    value: &'v mut serde_json::Value,
    pointer: &[&str],
) -> Option<&'v mut serde_json::Value> {
    let mut current = value;
    for key in pointer {
        current = current.get_mut(*key)?;
    }
    Some(current)
}

/// A recognizable, greppable placeholder that survives a `--resume` and
/// makes it obvious content was deliberately evicted (not just short),
/// pointing back at how to recover it — never a silent gap.
fn stub_text(original: &str) -> String {
    let preview: String = original.chars().take(STUB_PREVIEW_CHARS).collect();
    let truncated = original.chars().count() > STUB_PREVIEW_CHARS;
    format!(
        "[ng-evicted: {preview}{} — recupere com `ng search`]",
        if truncated { "…" } else { "" }
    )
}

/// Turn a list of 1-based `stubs` line numbers into `(line, replacement)`
/// pairs for [`ng_sessions::rewrite::rewrite_jsonl`], splitting `original`
/// with the *same* canonical [`split_lines`] the rewrite path uses so this
/// module and the rewriter can never disagree about which physical line a
/// number addresses (see the module doc for the finding-19b rationale). Pure
/// and synchronous so the line-targeting logic is unit-testable without an
/// HTTP server, discovery, or the security middleware.
pub(crate) fn build_stub_replacements(
    original: &str,
    stubs: &[usize],
    harness: &str,
) -> Result<Vec<(usize, String)>, String> {
    let lines = split_lines(original);
    let mut replacements = Vec::with_capacity(stubs.len());
    for line_no in stubs {
        let Some(line) = line_no.checked_sub(1).and_then(|i| lines.get(i)) else {
            return Err(format!("linha {line_no} fora do arquivo"));
        };
        let replacement = stub_replacement_line(line, harness)?;
        replacements.push((*line_no, replacement));
    }
    Ok(replacements)
}

/// Builds the replacement JSON for one JSONL line: swap its content field
/// for an eviction stub while leaving every other field (uuid, timestamps,
/// role, ids, ...) byte-identical. Only `stubs` in the UI's `RewriteRequest`
/// uses this; the frontend never has to know each harness's wire shape.
pub(crate) fn stub_replacement_line(original_line: &str, harness: &str) -> Result<String, String> {
    let mut value: serde_json::Value = serde_json::from_str(original_line)
        .map_err(|e| format!("linha original não é JSON válido: {e}"))?;

    let pointers = content_pointers(harness);
    let mut chosen: Option<(Vec<&str>, String, bool)> = None;
    for pointer in &pointers {
        if let Some(existing) = walk(&value, pointer) {
            if !existing.is_null() {
                let preview_source = match existing {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                chosen = Some((pointer.clone(), preview_source, existing.is_array()));
                break;
            }
        }
    }

    let Some((pointer, preview_source, was_array)) = chosen else {
        return Err(format!(
            "linha sem campo de conteúdo reconhecido para harness '{harness}'"
        ));
    };

    let stub = stub_text(&preview_source);
    // conteúdo em array (ex.: turn.prompt do kimi) mantém a forma de array
    // — um bloco de texto único em vez de uma string crua.
    let stub_value = if was_array {
        serde_json::json!([{ "type": "text", "text": stub }])
    } else {
        serde_json::Value::String(stub)
    };
    match walk_mut(&mut value, &pointer) {
        Some(target) => *target = stub_value,
        None => return Err("falha interna localizando campo de conteúdo".to_string()),
    }

    serde_json::to_string(&value).map_err(|e| format!("erro serializando substituição: {e}"))
}

/// Turn a list of `(1-based line, new content text)` edits into
/// `(line, replacement)` pairs with the same line-targeting discipline as
/// [`build_stub_replacements`]. The content field of the target line is
/// swapped for the user's text — every other field stays byte-identical.
/// String content stays a string; array content (ex.: blocos
/// `tool_result` do claude-code) vira um único bloco `{"type":"text"}`,
/// mantendo o envelope válido para o harness.
pub(crate) fn build_edit_replacements(
    original: &str,
    edits: &[(usize, String)],
    harness: &str,
) -> Result<Vec<(usize, String)>, String> {
    let lines = split_lines(original);
    let mut replacements = Vec::with_capacity(edits.len());
    for (line_no, new_text) in edits {
        let Some(line) = line_no.checked_sub(1).and_then(|i| lines.get(i)) else {
            return Err(format!("linha {line_no} fora do arquivo"));
        };
        replacements.push((*line_no, edit_replacement_line(line, harness, new_text)?));
    }
    Ok(replacements)
}

/// Builds the replacement JSON for one manual edit: swap the line's
/// content field for `new_text`, preserving every other field.
pub(crate) fn edit_replacement_line(
    original_line: &str,
    harness: &str,
    new_text: &str,
) -> Result<String, String> {
    let mut value: serde_json::Value = serde_json::from_str(original_line)
        .map_err(|e| format!("linha original não é JSON válido: {e}"))?;

    let pointers = content_pointers(harness);
    let mut chosen: Option<(Vec<&str>, bool)> = None;
    for pointer in &pointers {
        if let Some(existing) = walk(&value, pointer) {
            if !existing.is_null() {
                chosen = Some((pointer.clone(), existing.is_array()));
                break;
            }
        }
    }

    let Some((pointer, was_array)) = chosen else {
        return Err(format!(
            "linha sem campo de conteúdo reconhecido para harness '{harness}'"
        ));
    };

    let replacement_value = if was_array {
        serde_json::json!([{ "type": "text", "text": new_text }])
    } else {
        serde_json::Value::String(new_text.to_string())
    };
    match walk_mut(&mut value, &pointer) {
        Some(target) => *target = replacement_value,
        None => return Err("falha interna localizando campo de conteúdo".to_string()),
    }

    serde_json::to_string(&value).map_err(|e| format!("erro serializando edição: {e}"))
}

#[test]
fn edit_replacement_string_content_stays_string() {
    let line = r#"{"type":"assistant","uuid":"abc-123","message":{"role":"assistant","content":"texto velho"}}"#;
    let out = edit_replacement_line(line, "claude-code", "texto novo editado à mão").unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["uuid"], "abc-123");
    assert_eq!(value["message"]["content"], "texto novo editado à mão");
}

#[test]
fn edit_replacement_array_content_becomes_single_text_block() {
    let line = r#"{"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"saída gigante"}]}}"#;
    let out = edit_replacement_line(line, "claude-code", "resumo manual").unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["uuid"], "u1");
    let content = &value["message"]["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "resumo manual");
}

#[test]
fn build_edit_replacements_targets_physical_lines() {
    let original = "{\"a\":1}\n{\"content\":\"x\"}\n";
    let edits = vec![(2usize, "y".to_string())];
    let out = build_edit_replacements(original, &edits, "kimi").unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 2);
    let value: serde_json::Value = serde_json::from_str(&out[0].1).unwrap();
    assert_eq!(value["content"], "y");
    assert!(build_edit_replacements(original, &[(9, "z".into())], "kimi").is_err());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ng_sessions::rewrite::rewrite_jsonl;

    #[test]
    fn stub_replacement_preserves_other_fields_claude() {
        let line = r#"{"type":"assistant","uuid":"abc-123","message":{"role":"assistant","content":"segredo super longo que ninguem deveria ver de novo"}}"#;
        let out = stub_replacement_line(line, "claude-code").unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["uuid"], "abc-123");
        assert_eq!(value["type"], "assistant");
        assert_eq!(value["message"]["role"], "assistant");
        let content = value["message"]["content"].as_str().unwrap();
        assert!(content.starts_with("[ng-evicted:"));
        assert!(content.contains("segredo"));
    }

    #[test]
    fn stub_replacement_codex_payload_content() {
        let line = r#"{"payload":{"type":"message","role":"user","content":"dados sensiveis do usuario"},"timestamp":"t1"}"#;
        let out = stub_replacement_line(line, "codex").unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["timestamp"], "t1");
        assert_eq!(value["payload"]["type"], "message");
        assert!(value["payload"]["content"]
            .as_str()
            .unwrap()
            .starts_with("[ng-evicted:"));
    }

    #[test]
    fn stub_replacement_codex_falls_back_to_output() {
        let line = r#"{"payload":{"type":"function_call_output","output":"resultado sensivel"}}"#;
        let out = stub_replacement_line(line, "codex").unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(value["payload"]["output"]
            .as_str()
            .unwrap()
            .starts_with("[ng-evicted:"));
    }

    #[test]
    fn stub_replacement_kimi_top_level_content() {
        let line = r#"{"role":"user","content":"mensagem original"}"#;
        let out = stub_replacement_line(line, "kimi").unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["role"], "user");
        assert!(value["content"]
            .as_str()
            .unwrap()
            .starts_with("[ng-evicted:"));
    }

    #[test]
    fn stub_replacement_unknown_shape_errors() {
        let line = r#"{"foo":"bar"}"#;
        assert!(stub_replacement_line(line, "kimi").is_err());
    }

    #[test]
    fn stub_replacement_invalid_json_errors() {
        assert!(stub_replacement_line("not json", "claude-code").is_err());
    }

    #[test]
    fn build_stub_replacements_rejects_out_of_range_line() {
        let original = "{\"message\":{\"content\":\"a\"}}\n";
        assert!(build_stub_replacements(original, &[2], "claude-code").is_err());
        assert!(build_stub_replacements(original, &[0], "claude-code").is_err());
    }

    // [finding 19b] The UI stub path and the canonical rewriter must agree on
    // which physical line a 1-based number targets, even for CRLF transcripts
    // where `str::lines()` (old handler split) and `split_lines` (rewriter
    // split) differ in `\r` handling. Building the stub via
    // `build_stub_replacements` (now `split_lines`-based) and then applying it
    // via `rewrite_jsonl` must stub exactly the intended line and leave the
    // others byte-intact.
    #[test]
    fn ui_stub_path_and_canonical_rewrite_agree_on_line_targeting_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        // CRLF line endings, no trailing newline — the shape most likely to
        // expose a split divergence.
        let original = concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"linha um\"}}\r\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"linha dois secreta\"}}\r\n",
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"linha tres\"}}"
        );
        std::fs::write(&path, original).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let replacements = build_stub_replacements(&content, &[2], "claude-code").unwrap();
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].0, 2, "must address the 2nd line");

        rewrite_jsonl(&path, &[], &replacements).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let lines = split_lines(&after);
        assert_eq!(lines.len(), 3, "line count unchanged");

        let l1: serde_json::Value = serde_json::from_str(lines[0].trim_end_matches('\r')).unwrap();
        assert_eq!(l1["message"]["content"], "linha um", "line 1 intact");

        let l2: serde_json::Value = serde_json::from_str(lines[1].trim_end_matches('\r')).unwrap();
        let stubbed = l2["message"]["content"].as_str().unwrap();
        assert!(stubbed.starts_with("[ng-evicted:"), "line 2 stubbed");
        assert!(
            stubbed.contains("secreta"),
            "stub keeps a recognizable preview"
        );

        let l3: serde_json::Value = serde_json::from_str(lines[2].trim_end_matches('\r')).unwrap();
        assert_eq!(l3["message"]["content"], "linha tres", "line 3 intact");
    }

    #[test]
    fn stub_text_truncates_long_content_with_ellipsis() {
        let long = "x".repeat(200);
        let stub = stub_text(&long);
        assert!(stub.contains('…'));
        assert!(stub.starts_with("[ng-evicted:"));
    }

    #[test]
    fn stub_text_short_content_has_no_ellipsis() {
        let stub = stub_text("curto");
        assert!(!stub.contains('…'));
        assert!(stub.contains("curto"));
    }
}
