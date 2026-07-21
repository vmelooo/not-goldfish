//! Codex CLI transcripts: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Each line wraps a `ResponseItem` in `payload`: `message` (role + content
//! block array), `function_call` (tool invocation), or
//! `function_call_output` (tool result). Lines without a recognizable
//! `payload.type` (session metadata, reasoning traces, ...) become `other`.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::model::{SessionInfo, SessionItem, Transcript};
use crate::{extract_content_text, item_timestamp, preview, size_hint, tokens_est, Result};

pub const HARNESS: &str = "codex";

/// Find every `rollout-*.jsonl` under `~/.codex/sessions/**`.
pub fn discover(home: &Path) -> Vec<SessionInfo> {
    let root = home.join(".codex").join("sessions");
    let mut out = Vec::new();
    walk_rollouts(&root, &mut out);
    out
}

fn walk_rollouts(dir: &Path, out: &mut Vec<SessionInfo>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rollouts(&path, out);
            continue;
        }
        let is_rollout = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
            .unwrap_or(false);
        if !is_rollout {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let modified_at = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        out.push(SessionInfo {
            id,
            harness: HARNESS.to_string(),
            items_hint: size_hint(&path),
            path,
            project: None,
            modified_at,
        });
    }
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
        items.push(item_from_value(items.len(), raw_line, &value));
    }

    Ok(Transcript {
        info: info.clone(),
        items,
        skipped,
    })
}

fn item_from_value(index: usize, raw_line: usize, value: &Value) -> SessionItem {
    let payload = value.get("payload");
    let payload_type = payload
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("other");

    let (role, kind, text) = match payload_type {
        "message" => {
            let role = payload
                .and_then(|p| p.get("role"))
                .and_then(|r| r.as_str())
                .unwrap_or("other")
                .to_string();
            let (text, kind) = payload
                .and_then(|p| p.get("content"))
                .map(extract_content_text)
                .unwrap_or_default();
            (role, kind, text)
        }
        "function_call" => {
            let name = payload
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?");
            let args = payload
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("");
            (
                "tool".to_string(),
                "function_call".to_string(),
                format!("[{name}] {args}"),
            )
        }
        "function_call_output" => {
            let output = payload
                .and_then(|p| p.get("output"))
                .and_then(|o| o.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    payload
                        .and_then(|p| p.get("content"))
                        .map(|c| extract_content_text(c).0)
                })
                .unwrap_or_default();
            (
                "tool".to_string(),
                "function_call_output".to_string(),
                output,
            )
        }
        _ => ("other".to_string(), "other".to_string(), String::new()),
    };

    let text_full = if text.is_empty() {
        value.to_string()
    } else {
        text
    };

    SessionItem {
        index,
        role,
        kind,
        tokens_est: tokens_est(&text_full),
        text_preview: preview(&text_full),
        text_full,
        timestamp: item_timestamp(value),
        raw_line: Some(raw_line),
    }
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
    fn item_from_value_extracts_timestamp_when_present() {
        let value: Value = serde_json::from_str(
            r#"{"timestamp":"2026-07-18T10:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
        )
        .unwrap();
        let item = item_from_value(0, 1, &value);
        assert_eq!(item.timestamp, Some(1_784_368_801));
    }

    #[test]
    fn item_from_value_timestamp_is_none_when_absent() {
        let value: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
        )
        .unwrap();
        let item = item_from_value(0, 1, &value);
        assert_eq!(item.timestamp, None);
    }
}
