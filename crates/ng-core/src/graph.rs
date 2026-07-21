//! Wisdom graph: entities (files, concepts, errors, decisions) and the
//! relations between them, built incrementally from captured events.
//!
//! Extraction here is pure lexical heuristics — no ML, mirrors [`crate::lex`]
//! in spirit. Storage and graph traversal live on [`crate::Store`] since they
//! need the database; this module only decides *what* an event is about.

use std::collections::HashSet;

use crate::event::Event;

/// One entity extracted from an event: `(name, kind)`. `kind` is one of
/// `"file"`, `"concept"`, `"error"`, `"decision"`.
pub type ExtractedEntity = (String, String);

const MAX_CONCEPTS_PER_EVENT: usize = 5;

/// Decision markers are whole phrases (multi-word markers are inherently
/// word-bounded; single-word ones get an explicit boundary check). The old
/// bare substrings "use ", "sempre", "nunca" produced spurious decisions
/// ("how do I use this?") — a decision node starts at weight 3.0, the
/// highest in the graph, so false positives here poison rankings hardest.
const DECISION_MARKERS: &[&str] = &[
    "decidimos",
    "vamos usar",
    "vamos de",
    "escolhi",
    "a partir de agora",
    "sempre que",
    "nunca mais",
    "prefiro",
    "fica decidido",
];

/// How many leading words become a decision entity's name — enough to be
/// recognizable in an export, short enough to stay a label, not a quote.
const DECISION_NAME_WORDS: usize = 8;

/// Extract the entities an event is "about". Dispatch by kind — this is
/// the wisdom graph's single poison gate:
/// - `prompt` / `assistant` (dialogue): lexical concepts from tags, plus
///   file/error tags, plus (prompt only) decision detection. Skipped when
///   the content is mostly pasted code/JSON/SQL.
/// - `tool_output`: typed entities only (structured meta + error
///   signatures) — see [`extract_typed_entities`]; never lexical tags.
/// - everything else (`session_*`, `precompact`, `ng_meta`, `system`,
///   `other`, unknown kinds): nothing.
pub fn extract_entities(event: &Event) -> Vec<ExtractedEntity> {
    match event.kind.as_str() {
        "prompt" | "assistant" => extract_dialogue_entities(event),
        "tool_output" => extract_typed_entities(event),
        _ => Vec::new(),
    }
}

fn extract_dialogue_entities(event: &Event) -> Vec<ExtractedEntity> {
    let mut out: Vec<ExtractedEntity> = Vec::new();
    let mut seen: HashSet<ExtractedEntity> = HashSet::new();

    if !crate::lex::is_mostly_code(&event.content) {
        let mut concepts: Vec<&str> = Vec::new();
        for tag in event.tags.split_whitespace() {
            if tag.contains('/') {
                push_unique(&mut out, &mut seen, tag.to_string(), "file");
            } else if is_error_tag(tag) {
                push_unique(&mut out, &mut seen, tag.to_string(), "error");
            } else {
                concepts.push(tag);
            }
        }
        for tag in concepts.into_iter().take(MAX_CONCEPTS_PER_EVENT) {
            push_unique(&mut out, &mut seen, tag.to_string(), "concept");
        }
    }

    if event.kind == "prompt" {
        if let Some(name) = decision_from_prompt(&event.content) {
            push_unique(&mut out, &mut seen, name, "decision");
        }
    }
    out
}

const MAX_ERROR_ENTITIES_PER_EVENT: usize = 2;
const ERROR_SIGNATURE_WORDS: usize = 6;
const ERROR_SIGNATURE_MAX_CHARS: usize = 60;

/// Typed extraction for tool outputs: never lexical. `file` comes from the
/// hook's structured meta (`file_path` of Read/Edit/Write), `error` from a
/// dedicated recognizer over the output text. Anything the recognizers
/// don't claim contributes nothing — a passing `ls` output is graph-inert.
fn extract_typed_entities(event: &Event) -> Vec<ExtractedEntity> {
    let mut out: Vec<ExtractedEntity> = Vec::new();
    let mut seen: HashSet<ExtractedEntity> = HashSet::new();

    if let Some(meta) = event
        .meta
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
    {
        if let Some(path) = meta.get("file_path").and_then(|v| v.as_str()) {
            push_unique(&mut out, &mut seen, path.to_string(), "file");
        }
    }

    let mut errors = 0usize;
    for line in event.content.lines() {
        if errors >= MAX_ERROR_ENTITIES_PER_EVENT {
            break;
        }
        if let Some(signature) = error_signature(line) {
            if seen.insert((signature.clone(), "error".to_string())) {
                out.push((signature, "error".to_string()));
                errors += 1;
            }
        }
    }
    out
}

/// A line is an error line if it carries an `E\d{3,5}` code (the code IS
/// the signature — stable across occurrences) or starts an error report
/// ("error:", "error[", "panicked at", "FAILED"). `FAILED` is matched
/// case-sensitively: the summary of every successful cargo test run says
/// `0 failed;`, and that must not read as an error. Signature = first
/// [`ERROR_SIGNATURE_WORDS`] words, capped at [`ERROR_SIGNATURE_MAX_CHARS`].
/// The hook frames tool output as `[Tool] input: …\noutput: …`, so a
/// leading `output:` is envelope, not error text — dropped before both
/// checks so the signature names the error itself.
fn error_signature(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix("output:").unwrap_or(trimmed).trim();
    if let Some(code) = trimmed
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|token| is_error_code(token))
    {
        return Some(code.to_string());
    }
    let lower = trimmed.to_lowercase();
    let is_error_line = lower.starts_with("error:")
        || lower.starts_with("error[")
        || lower.contains("panicked at")
        || trimmed.contains("FAILED");
    if !is_error_line {
        return None;
    }
    let mut signature = trimmed
        .split_whitespace()
        .take(ERROR_SIGNATURE_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    if signature.len() > ERROR_SIGNATURE_MAX_CHARS {
        let mut cut = ERROR_SIGNATURE_MAX_CHARS;
        while !signature.is_char_boundary(cut) {
            cut -= 1;
        }
        signature.truncate(cut);
    }
    (!signature.is_empty()).then_some(signature)
}

fn decision_from_prompt(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    let hit = DECISION_MARKERS.iter().any(|marker| {
        lower
            .match_indices(marker)
            .any(|(at, _)| is_word_bounded(&lower, at, marker.len()))
    });
    if !hit {
        return None;
    }
    let name = decision_name(content);
    (!name.is_empty()).then_some(name)
}

/// `lower[at .. at+len]` sits on word boundaries on both sides.
fn is_word_bounded(lower: &str, at: usize, len: usize) -> bool {
    let before_ok = at == 0
        || lower[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
    let after_ok = lower[at + len..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric());
    before_ok && after_ok
}

fn push_unique(
    out: &mut Vec<ExtractedEntity>,
    seen: &mut HashSet<ExtractedEntity>,
    name: String,
    kind: &str,
) {
    let key = (name, kind.to_string());
    if seen.insert(key.clone()) {
        out.push(key);
    }
}

/// First ~[`DECISION_NAME_WORDS`] words of `content`, used as a decision
/// entity's human-readable label.
fn decision_name(content: &str) -> String {
    content
        .split_whitespace()
        .take(DECISION_NAME_WORDS)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A tag reads as an error marker either by keyword (en + pt-BR) or by
/// shape — a bare error code like `E0502` or `E404`. Substring matching
/// on "erro" can false-positive on unrelated words that happen to contain
/// it (e.g. "terror"); acceptable for a lexical heuristic feeding a weight
/// that only ever nudges ranking, never gates correctness.
fn is_error_tag(tag: &str) -> bool {
    let lower = tag.to_lowercase();
    if lower.contains("error")
        || lower.contains("panic")
        || lower.contains("failed")
        || lower.contains("erro")
    {
        return true;
    }
    is_error_code(tag)
}

/// `E` (or `e`) followed by 3–5 digits, e.g. `E0502`, `E404`.
fn is_error_code(tag: &str) -> bool {
    let mut chars = tag.chars();
    match chars.next() {
        Some('E') | Some('e') => {
            let rest: String = chars.collect();
            (3..=5).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, content: &str, tags: &str) -> Event {
        Event {
            session_id: "s1".into(),
            project: "/tmp/proj".into(),
            harness: "claude-code".into(),
            kind: kind.into(),
            content: content.into(),
            tags: tags.into(),
            meta: None,
            created_at: 1_700_000_000,
        }
    }

    fn ev_meta(kind: &str, content: &str, meta: &str) -> Event {
        let mut e = ev(kind, content, "");
        e.meta = Some(meta.to_string());
        e
    }

    #[test]
    fn tool_output_file_entity_comes_from_meta_not_tags() {
        let e = ev_meta(
            "tool_output",
            "[Read] input: {}\noutput: conteudo qualquer",
            r#"{"tool":"Read","file_path":"/proj/src/store.rs"}"#,
        );
        let entities = extract_entities(&e);
        assert_eq!(
            entities,
            vec![("/proj/src/store.rs".to_string(), "file".to_string())]
        );
    }

    #[test]
    fn tool_output_error_signature_from_content() {
        let e = ev_meta(
            "tool_output",
            "[Bash] input: {\"command\":\"cargo build\"}\noutput: error[E0502]: cannot borrow `x` as mutable\nmore context here",
            r#"{"tool":"Bash"}"#,
        );
        let entities = extract_entities(&e);
        assert!(entities.contains(&("E0502".to_string(), "error".to_string())));
    }

    #[test]
    fn tool_output_error_line_without_code_uses_short_signature() {
        let e = ev_meta(
            "tool_output",
            "[Bash] input: {}\noutput: thread 'main' panicked at src/db.rs:42: index out of bounds",
            r#"{"tool":"Bash"}"#,
        );
        let entities = extract_entities(&e);
        let errors: Vec<&str> = entities
            .iter()
            .filter(|(_, k)| k == "error")
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("thread 'main' panicked at"));
        assert!(errors[0].len() <= 60);
    }

    #[test]
    fn tool_output_caps_error_entities_at_two() {
        let content = "[Bash] input: {}\noutput: error: a\nerror: b\nerror: c\nerror: d";
        let e = ev_meta("tool_output", content, r#"{"tool":"Bash"}"#);
        let errors = extract_entities(&e)
            .into_iter()
            .filter(|(_, k)| k == "error")
            .count();
        assert_eq!(errors, 2);
    }

    #[test]
    fn tool_output_success_output_yields_nothing_without_meta_path() {
        let e = ev_meta(
            "tool_output",
            "[Bash] input: {\"command\":\"ls\"}\noutput: Cargo.toml src target",
            r#"{"tool":"Bash"}"#,
        );
        assert!(extract_entities(&e).is_empty());
    }

    #[test]
    fn tool_output_cargo_test_success_summary_is_not_an_error() {
        let e = ev_meta(
            "tool_output",
            "[Bash] input: {}\noutput: test result: ok. 101 passed; 0 failed; 0 ignored",
            r#"{"tool":"Bash"}"#,
        );
        assert!(extract_entities(&e).is_empty());
    }

    #[test]
    fn tool_output_uppercase_failed_test_line_yields_error() {
        let e = ev_meta(
            "tool_output",
            "[Bash] input: {}\noutput: test graph::tests::foo ... FAILED",
            r#"{"tool":"Bash"}"#,
        );
        let errors = extract_entities(&e)
            .into_iter()
            .filter(|(_, k)| k == "error")
            .count();
        assert_eq!(errors, 1);
    }

    #[test]
    fn tool_output_invalid_meta_json_is_ignored() {
        let e = ev_meta("tool_output", "output: ok", "not json");
        assert!(extract_entities(&e).is_empty());
    }

    #[test]
    fn tool_output_produces_no_lexical_entities() {
        let e = ev("tool_output", "build ok", "src/auth/login.rs cargo build");
        assert!(extract_entities(&e).is_empty());
    }

    #[test]
    fn prompt_tags_yield_concepts_files_and_errors() {
        let e = ev(
            "prompt",
            "o build quebrou de novo",
            "panic E0502 src/db.rs timeout",
        );
        let entities = extract_entities(&e);
        assert!(entities.contains(&("panic".to_string(), "error".to_string())));
        assert!(entities.contains(&("E0502".to_string(), "error".to_string())));
        assert!(entities.contains(&("src/db.rs".to_string(), "file".to_string())));
        assert!(entities.contains(&("timeout".to_string(), "concept".to_string())));
    }

    #[test]
    fn caps_concepts_at_five_and_keeps_tag_order() {
        let e = ev("prompt", "noop", "um dois tres quatro cinco seis sete");
        let entities = extract_entities(&e);
        let concepts: Vec<&str> = entities
            .iter()
            .filter(|(_, kind)| kind == "concept")
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(concepts, vec!["um", "dois", "tres", "quatro", "cinco"]);
    }

    #[test]
    fn assistant_produces_concepts_but_never_decisions() {
        let e = ev(
            "assistant",
            "vamos usar rusqlite no projeto",
            "rusqlite projeto banco",
        );
        let entities = extract_entities(&e);
        assert!(entities.iter().any(|(_, k)| k == "concept"));
        assert!(!entities.iter().any(|(_, k)| k == "decision"));
    }

    #[test]
    fn marker_kinds_produce_nothing() {
        for kind in [
            "session_start",
            "session_end",
            "precompact",
            "ng_meta",
            "system",
            "other",
        ] {
            let e = ev(kind, "algum texto com src/x.rs", "src/x.rs texto");
            assert!(
                extract_entities(&e).is_empty(),
                "kind {kind} vazou pro grafo"
            );
        }
    }

    #[test]
    fn mostly_code_prompt_yields_no_concepts_but_decision_still_works() {
        let sql = "decidimos manter esta query: \
                   SELECT id, session_id FROM events WHERE id > ?1 ORDER BY id LIMIT ?2; \
                   INSERT INTO entities (name, kind) VALUES ('x','y'); UPDATE relations SET weight = 1.0;";
        let e = ev("prompt", sql, "select session_id events entities relations");
        let entities = extract_entities(&e);
        assert!(
            !entities.iter().any(|(_, k)| k == "concept"),
            "SQL virou concept"
        );
        assert!(
            entities.iter().any(|(_, k)| k == "decision"),
            "marcador de decisão ignorado num prompt mostly-code"
        );
    }

    #[test]
    fn decision_markers_require_word_boundaries() {
        // Falsos positivos do contrato antigo:
        assert!(
            extract_entities(&ev("prompt", "how do I use this crate?", ""))
                .iter()
                .all(|(_, k)| k != "decision")
        );
        assert!(
            extract_entities(&ev("prompt", "o museu sempre abre cedo", ""))
                .iter()
                .all(|(_, k)| k != "decision")
        );
        assert!(
            extract_entities(&ev("prompt", "isso nunca funciona direito", ""))
                .iter()
                .all(|(_, k)| k != "decision")
        );

        // Positivos reais:
        for prompt in [
            "decidimos migrar para axum na proxima sprint",
            "vamos usar rusqlite para o banco",
            "escolhi o layout radial para o grafo",
            "a partir de agora todo commit passa por clippy",
            "sempre que o daemon cair, use o fallback direto",
            "nunca mais coloque segredo em log",
            "prefiro manter o arquivo unico",
        ] {
            assert!(
                extract_entities(&ev("prompt", prompt, ""))
                    .iter()
                    .any(|(_, k)| k == "decision"),
                "não detectou decisão em: {prompt}"
            );
        }
    }

    #[test]
    fn extracts_decision_from_prompt_with_marker() {
        let e = ev(
            "prompt",
            "vamos usar rusqlite para o banco de dados local sempre",
            "",
        );
        let entities = extract_entities(&e);
        let decisions: Vec<&str> = entities
            .iter()
            .filter(|(_, kind)| kind == "decision")
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0].starts_with("vamos usar rusqlite"));
    }

    #[test]
    fn non_prompt_events_never_produce_decisions() {
        let e = ev("tool_output", "vamos usar rusqlite sempre", "");
        let entities = extract_entities(&e);
        assert!(!entities.iter().any(|(_, kind)| kind == "decision"));
    }

    #[test]
    fn prompt_without_marker_has_no_decision() {
        let e = ev("prompt", "qual é o status do build agora", "");
        let entities = extract_entities(&e);
        assert!(!entities.iter().any(|(_, kind)| kind == "decision"));
    }

    #[test]
    fn empty_event_yields_no_entities() {
        let e = ev("prompt", "", "");
        assert!(extract_entities(&e).is_empty());
    }
}
