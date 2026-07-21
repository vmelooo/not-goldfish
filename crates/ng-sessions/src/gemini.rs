//! Gemini CLI transcripts: `~/.gemini/tmp/<hash>/chats/*.json`.
//!
//! Each file is a single JSON document with a `history` (or legacy
//! `messages`) array of `{ role, parts: [...] }`. Not line-addressable, so
//! items never carry a `raw_line` and cannot be targeted by `rewrite.rs`.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::model::{SessionInfo, SessionItem, Transcript};
use crate::{preview, size_hint, tokens_est, Result};

pub const HARNESS: &str = "gemini";

/// Find every `*.json` chat file under `~/.gemini/tmp/*/chats/`.
pub fn discover(home: &Path) -> Vec<SessionInfo> {
    let root = home.join(".gemini").join("tmp");
    let mut out = Vec::new();
    let Ok(hash_dirs) = fs::read_dir(&root) else {
        return out;
    };
    for hash_dir in hash_dirs.flatten() {
        let chats = hash_dir.path().join("chats");
        let Ok(files) = fs::read_dir(&chats) else {
            continue;
        };
        for entry in files.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
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
    out
}

pub fn parse(info: &SessionInfo) -> Result<Transcript> {
    let raw = fs::read_to_string(&info.path)?;
    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            // The whole document is malformed: nothing to salvage line by
            // line in a single-JSON format, so the entire file counts as
            // skipped rather than erroring out the caller.
            return Ok(Transcript {
                info: info.clone(),
                items: Vec::new(),
                skipped: 1,
            });
        }
    };

    let entries = value
        .get("history")
        .or_else(|| value.get("messages"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let items = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| item_from_value(index, entry))
        .collect();

    Ok(Transcript {
        info: info.clone(),
        items,
        skipped: 0,
    })
}

fn item_from_value(index: usize, value: &Value) -> SessionItem {
    let role = value
        .get("role")
        .and_then(|r| r.as_str())
        .map(normalize_role)
        .unwrap_or_else(|| "other".to_string());

    let mut text = String::new();
    let mut kind = "other".to_string();
    if let Some(parts) = value.get("parts").and_then(|p| p.as_array()) {
        for part in parts {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
                kind = "text".to_string();
            } else if part.get("functionCall").is_some() {
                kind = "tool_use".to_string();
            } else if part.get("functionResponse").is_some() {
                kind = "tool_result".to_string();
            }
        }
    }

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
        // Gemini's `history`/`messages` entries have no per-item timestamp
        // field to extract; the watcher falls back to file mtime.
        timestamp: None,
        raw_line: None,
    }
}

fn normalize_role(role: &str) -> String {
    match role {
        "user" => "user".to_string(),
        "model" | "assistant" => "assistant".to_string(),
        "system" => "system".to_string(),
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
}
