//! Integration tests for `ng_hook::precompact::handle_precompact`.
//!
//! Uses a synthetic Claude Code JSONL transcript, sized so a single large
//! tool_result already exceeds the default hygiene token target — no need
//! to touch `NG_HYGIENE_TARGET_TOKENS` (mutating process-global env state
//! from a parallel test binary would be racy).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ng_hook::precompact::handle_precompact;

/// 25 items: a real user prompt (never evicted), one huge tool_result
/// outside the hot zone (evictable, and alone big enough to blow past the
/// default 20_000-token hygiene target), and 20 small hot-zone items
/// (never evicted regardless of size/kind).
fn write_synthetic_transcript(dir: &Path) -> PathBuf {
    let path = dir.join("session.jsonl");
    let huge = "y".repeat(90_000); // ~22_500 estimated tokens

    let mut f = fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"{{"type":"user","message":{{"role":"user","content":"Please clean up the old debug logging."}},"uuid":"u0"}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{huge}"}}]}},"uuid":"u1","parentUuid":"u0"}}"#
    )
    .unwrap();
    for i in 2..5 {
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"ok {i}"}},"uuid":"pad{i}"}}"#
        )
        .unwrap();
    }
    for i in 5..25 {
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"recent {i}"}},"uuid":"hot{i}"}}"#
        )
        .unwrap();
    }
    path
}

fn precompact_payload(transcript_path: &Path) -> String {
    serde_json::json!({
        "hook_event_name": "PreCompact",
        "transcript_path": transcript_path.to_string_lossy(),
        "session_id": "sess-test-1",
        "trigger": "auto",
    })
    .to_string()
}

#[test]
fn enabled_gate_stubs_the_huge_tool_result_and_reports_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_synthetic_transcript(tmp.path());
    let before = fs::read(&path).unwrap();
    let payload = precompact_payload(&path);

    let outcome = handle_precompact(&payload, true).expect("should stub the huge tool_result");
    let response: serde_json::Value =
        serde_json::from_str(&outcome.response).expect("output must be valid JSON");
    assert_eq!(
        response["hookSpecificOutput"]["hookEventName"],
        "PreCompact"
    );
    let context = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("not-goldfish"));
    assert!(context.contains("tok liberados"));

    // The real transcript file was rewritten: the huge tool_result became
    // a stub, everything else (including the sacred user prompt) unchanged.
    let after = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = after.lines().collect();
    assert_eq!(lines.len(), 25, "line count must be preserved");

    let user_prompt: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        user_prompt["message"]["content"],
        "Please clean up the old debug logging."
    );

    let evicted: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(evicted["uuid"], "u1");
    assert_eq!(evicted["parentUuid"], "u0");
    let stub = evicted["message"]["content"].as_str().unwrap();
    assert!(stub.starts_with("[ng-evicted: tool_result"));
    assert!(stub.contains("ng search"));

    // A backup with the pre-rewrite bytes must exist somewhere in the
    // session's directory.
    let backup_files: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().contains("ng-bak"))
        .collect();
    assert_eq!(
        backup_files.len(),
        1,
        "exactly one backup should be written"
    );
    assert_eq!(fs::read(&backup_files[0]).unwrap(), before);
}

#[test]
fn disabled_gate_leaves_the_transcript_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_synthetic_transcript(tmp.path());
    let before = fs::read(&path).unwrap();
    let payload = precompact_payload(&path);

    let output = handle_precompact(&payload, false);
    assert!(output.is_none(), "gate off must produce no hook output");

    let after = fs::read(&path).unwrap();
    assert_eq!(before, after, "gate off must not touch the transcript file");

    // No backup should have been written either.
    let backups: Vec<_> = fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.path().to_string_lossy().contains("ng-bak"))
        .collect();
    assert!(backups.is_empty());
}

#[test]
fn non_precompact_event_is_ignored_even_when_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_synthetic_transcript(tmp.path());
    let payload = serde_json::json!({
        "hook_event_name": "PostToolUse",
        "transcript_path": path.to_string_lossy(),
        "session_id": "sess-test-1",
    })
    .to_string();

    assert!(handle_precompact(&payload, true).is_none());
}

#[test]
fn malformed_payload_never_panics() {
    for payload in ["not json", "{}", r#"{"hook_event_name":"PreCompact"}"#] {
        assert!(handle_precompact(payload, true).is_none());
    }
}
