//! opencode transcripts, split across two trees under
//! `~/.local/share/opencode/storage/`:
//!   - `session/<workspace>/<id>.json` — session info (one file per session).
//!   - `message/<id>/*.json` — one file per message, filename-sortable.
//!
//! A session with no message directory yet (freshly created) parses to zero
//! items rather than erroring.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::model::{SessionInfo, SessionItem, Transcript};
use crate::{extract_content_text, preview, size_hint, tokens_est, Result};

pub const HARNESS: &str = "opencode";

/// Find every `session/<workspace>/<id>.json` under the opencode storage dir.
pub fn discover(home: &Path) -> Vec<SessionInfo> {
    let storage = home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("storage");
    let session_root = storage.join("session");
    let message_root = storage.join("message");
    let mut out = Vec::new();

    let Ok(workspace_dirs) = fs::read_dir(&session_root) else {
        return out;
    };
    for workspace_dir in workspace_dirs.flatten() {
        let workspace_path = workspace_dir.path();
        if !workspace_path.is_dir() {
            continue;
        }
        let workspace_name = workspace_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
        let Ok(files) = fs::read_dir(&workspace_path) else {
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
            let items_hint = fs::read_dir(message_root.join(&id))
                .ok()
                .map(|entries| entries.flatten().count())
                .or_else(|| size_hint(&path));
            out.push(SessionInfo {
                id,
                harness: HARNESS.to_string(),
                items_hint,
                path,
                project: workspace_name.clone(),
                modified_at,
            });
        }
    }
    out
}

pub fn parse(info: &SessionInfo) -> Result<Transcript> {
    // storage/session/<workspace>/<id>.json -> storage root is two levels up.
    let storage_root = info
        .path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());

    let mut message_files: Vec<_> = storage_root
        .map(|root| root.join("message").join(&info.id))
        .and_then(|dir| fs::read_dir(dir).ok())
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    message_files.sort();

    let mut items = Vec::new();
    let mut skipped = 0usize;
    for path in message_files {
        let raw = match fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        items.push(item_from_value(items.len(), &value));
    }

    Ok(Transcript {
        info: info.clone(),
        items,
        skipped,
    })
}

fn item_from_value(index: usize, value: &Value) -> SessionItem {
    let role = value
        .get("role")
        .and_then(|r| r.as_str())
        .map(|r| match r {
            "user" | "assistant" | "system" | "tool" => r.to_string(),
            _ => "other".to_string(),
        })
        .unwrap_or_else(|| "other".to_string());

    let (text, kind) = match value.get("content") {
        Some(content) => extract_content_text(content),
        None => (String::new(), "other".to_string()),
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
        // opencode's message files use their own `time.created` (epoch
        // millis, not RFC3339) rather than a `timestamp` string field;
        // left unmapped for now, the watcher falls back to file mtime.
        timestamp: None,
        raw_line: None,
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
