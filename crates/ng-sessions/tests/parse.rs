//! Integration tests: one parser per harness format, driven by the
//! fixtures in `tests/fixtures/`, plus discovery against directories that
//! don't exist (must never fail or panic).

use std::path::PathBuf;
use std::time::SystemTime;

use ng_sessions::model::SessionInfo;
use ng_sessions::{claude, codex, gemini, kimi, opencode};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn info(harness: &str, id: &str, path: PathBuf, project: Option<&str>) -> SessionInfo {
    SessionInfo {
        id: id.to_string(),
        harness: harness.to_string(),
        path,
        project: project.map(|p| p.to_string()),
        modified_at: SystemTime::now(),
        items_hint: None,
    }
}

#[test]
fn claude_parses_tolerantly() {
    let path = fixtures_dir().join("claude.jsonl");
    let session = info("claude-code", "claude", path, Some("-home-dev-proj"));
    let transcript = claude::parse(&session).expect("claude fixture should parse");

    assert_eq!(
        transcript.skipped, 1,
        "one malformed line should be counted, not fatal"
    );
    assert_eq!(transcript.items.len(), 9);

    assert_eq!(transcript.items[0].role, "user");
    assert!(transcript.items[0].text_preview.contains("login bug"));
    assert_eq!(transcript.items[0].raw_line, Some(1));

    // text + tool_use blocks in one message -> mixed kind.
    assert_eq!(transcript.items[1].kind, "mixed");

    assert_eq!(transcript.items[2].kind, "tool_result");

    // Malformed line 5 was skipped, so line 6 lands at raw_line 6, not 5.
    let tool_use_only = &transcript.items[4];
    assert_eq!(tool_use_only.raw_line, Some(6));
    assert_eq!(tool_use_only.kind, "tool_use");

    // "summary" type has no `message` field -> downgraded, never fatal.
    let summary_item = transcript
        .items
        .iter()
        .find(|i| i.raw_line == Some(8))
        .unwrap();
    assert_eq!(summary_item.role, "system");

    // Unrecognized future type -> "other", not an error.
    let unknown = transcript.items.last().unwrap();
    assert_eq!(unknown.role, "other");
    assert_eq!(unknown.kind, "other");
    assert!(
        !unknown.text_preview.is_empty(),
        "falls back to raw JSON preview"
    );
}

#[test]
fn codex_parses_tolerantly() {
    let path = fixtures_dir().join("codex.jsonl");
    let session = info("codex", "codex-1", path, None);
    let transcript = codex::parse(&session).expect("codex fixture should parse");

    assert_eq!(transcript.skipped, 1);
    assert_eq!(transcript.items.len(), 7);

    let session_meta = &transcript.items[0];
    assert_eq!(session_meta.role, "other");

    let user_msg = &transcript.items[1];
    assert_eq!(user_msg.role, "user");
    assert!(user_msg.text_preview.contains("retry"));

    let call = transcript
        .items
        .iter()
        .find(|i| i.kind == "function_call")
        .unwrap();
    assert_eq!(call.role, "tool");
    assert!(call.text_preview.contains("shell"));

    let output = transcript
        .items
        .iter()
        .find(|i| i.kind == "function_call_output")
        .unwrap();
    assert!(output.text_preview.contains("pub fn get"));

    let reasoning = transcript.items.last().unwrap();
    assert_eq!(reasoning.role, "other");
    assert_eq!(reasoning.kind, "other");
}

#[test]
fn gemini_parses_tolerantly() {
    let path = fixtures_dir().join("gemini.json");
    let session = info("gemini", "gem-session-1", path, None);
    let transcript = gemini::parse(&session).expect("gemini fixture should parse");

    assert_eq!(transcript.skipped, 0);
    assert_eq!(transcript.items.len(), 9);
    assert_eq!(transcript.items[0].role, "user");
    assert_eq!(transcript.items[1].role, "assistant"); // "model" normalized
    assert!(transcript.items[1].text_preview.contains("context daemon"));

    let call = &transcript.items[3];
    assert_eq!(call.kind, "tool_use");
    let response = &transcript.items[4];
    assert_eq!(response.kind, "tool_result");

    // Entries with no role or empty parts never panic and still surface.
    let no_role = &transcript.items[7];
    assert_eq!(no_role.role, "other");
    let no_parts = &transcript.items[6];
    assert_eq!(no_parts.kind, "other");

    for item in &transcript.items {
        assert!(
            item.raw_line.is_none(),
            "single-JSON format has no line correspondence"
        );
    }
}

#[test]
fn opencode_parses_tolerantly() {
    let session_path = fixtures_dir().join("opencode/storage/session/my-workspace/ses_1.json");
    let session = info("opencode", "ses_1", session_path, Some("my-workspace"));
    let transcript = opencode::parse(&session).expect("opencode fixture should parse");

    assert_eq!(
        transcript.skipped, 1,
        "one broken message file should be counted, not fatal"
    );
    assert_eq!(transcript.items.len(), 3);

    assert_eq!(transcript.items[0].role, "user");
    assert!(transcript.items[0].text_preview.contains("pagination"));

    assert_eq!(transcript.items[1].role, "assistant");
    assert_eq!(transcript.items[1].kind, "mixed");

    assert_eq!(transcript.items[2].role, "assistant");
    assert!(transcript.items[2].text_preview.contains("tests pass"));
}

#[test]
fn kimi_parses_tolerantly() {
    let path = fixtures_dir().join("kimi.jsonl");
    let session = info("kimi", "kimi-1", path, Some("my-workspace"));
    let transcript = kimi::parse(&session).expect("kimi fixture should parse");

    assert_eq!(transcript.skipped, 1);
    assert_eq!(transcript.items.len(), 5);
    assert_eq!(transcript.items[0].role, "user");
    assert_eq!(transcript.items[2].role, "tool");
    assert_eq!(transcript.items[2].kind, "tool_use");
    let tool_result = &transcript.items[3];
    assert_eq!(tool_result.kind, "tool_result");
    assert!(tool_result.text_preview.contains("1.70"));
}

#[test]
fn claude_large_content_keeps_full_text_alongside_short_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("large.jsonl");
    let big = "word ".repeat(2000); // ~10_000 chars, way past PREVIEW_CHARS
    std::fs::write(
        &path,
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}},"uuid":"u1"}}"#,
            big.trim_end()
        ),
    )
    .unwrap();

    let session = info("claude-code", "large", path, None);
    let transcript = claude::parse(&session).expect("large fixture should parse");
    assert_eq!(transcript.items.len(), 1);

    let item = &transcript.items[0];
    assert_eq!(
        item.text_full,
        big.trim_end(),
        "text_full must carry the complete content"
    );
    assert!(
        item.text_preview.chars().count() <= ng_sessions::model::PREVIEW_CHARS,
        "text_preview must stay capped even when text_full is large"
    );
    assert!(item.text_full.len() > item.text_preview.len());
    assert_eq!(
        item.tokens_est,
        (item.text_full.len() / 4) as i64,
        "tokens_est must reflect the full text, not the preview"
    );
}

#[test]
fn discovery_never_fails_on_missing_dirs() {
    let empty = tempfile::tempdir().unwrap();
    assert!(claude::discover(empty.path()).is_empty());
    assert!(codex::discover(empty.path()).is_empty());
    assert!(gemini::discover(empty.path()).is_empty());
    assert!(opencode::discover(empty.path()).is_empty());
    assert!(kimi::discover(empty.path()).is_empty());
}

#[test]
fn discover_sessions_top_level_does_not_panic() {
    // Exercises the real dispatch against whatever harness dirs happen to
    // exist (or not) on the machine running the test.
    let _ = ng_sessions::discover_sessions();
}
