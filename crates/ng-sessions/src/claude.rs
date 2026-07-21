//! Claude Code transcripts: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`.
//!
//! Each line is a JSON object with `type` (`user`/`assistant`/`summary`/...),
//! a `message` object (`role`, `content` — string or block array), a `uuid`,
//! and a `cwd`. Every field is read defensively: harness versions drift.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::model::{SessionInfo, SessionItem, Transcript};
use crate::{extract_content_text, item_timestamp, preview, size_hint, tokens_est, Result};

pub const HARNESS: &str = "claude-code";

/// Find every `*.jsonl` transcript under `~/.claude/projects/*/`.
pub fn discover(home: &Path) -> Vec<SessionInfo> {
    let root = home.join(".claude").join("projects");
    let mut out = Vec::new();
    let Ok(project_dirs) = fs::read_dir(&root) else {
        return out;
    };
    for project_dir in project_dirs.flatten() {
        let dir_path = project_dir.path();
        if !dir_path.is_dir() {
            continue;
        }
        let project = dir_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        let Ok(files) = fs::read_dir(&dir_path) else {
            continue;
        };
        for entry in files.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
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
                project: project.clone(),
                modified_at,
            });
        }
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
        items.push(item_from_value(items.len(), raw_line, &value));
    }

    Ok(Transcript {
        info: info.clone(),
        items,
        skipped,
    })
}

fn item_from_value(index: usize, raw_line: usize, value: &Value) -> SessionItem {
    let msg_type = value.get("type").and_then(|t| t.as_str());
    let message = value.get("message");

    let role = message
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .or(msg_type)
        .map(normalize_role)
        .unwrap_or_else(|| "other".to_string());

    let (text, kind) = match message.and_then(|m| m.get("content")) {
        Some(content) => extract_content_text(content),
        None => (String::new(), "other".to_string()),
    };

    // When nothing recognizable was extracted, fall back to the raw JSON
    // of the whole item so `text_full`/`text_preview` still carry
    // something inspectable instead of going empty.
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

fn normalize_role(role: &str) -> String {
    match role {
        "user" | "assistant" | "system" | "tool" => role.to_string(),
        "summary" => "system".to_string(),
        _ => "other".to_string(),
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
            r#"{"type":"user","timestamp":"2026-07-18T10:00:00Z","message":{"role":"user","content":"hi"},"uuid":"u1"}"#,
        )
        .unwrap();
        let item = item_from_value(0, 1, &value);
        assert_eq!(item.timestamp, Some(1_784_368_800));
    }

    #[test]
    fn item_from_value_timestamp_is_none_when_absent() {
        let value: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1"}"#,
        )
        .unwrap();
        let item = item_from_value(0, 1, &value);
        assert_eq!(item.timestamp, None);
    }
}
