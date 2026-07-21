//! Transcript polling for harnesses with no hook API (Codex today; Grok is
//! the same shape once it needs one). Since there's nowhere to inject a
//! hook, not-goldfish falls back to periodically scanning each harness's
//! own transcript files and importing whatever is new since the last scan.
//!
//! Parsing itself is **not** reimplemented here — it reuses
//! `ng_sessions::load_transcript`, the same tolerant-by-`serde_json::Value`
//! parser the hook-based path benefits from, so a malformed line degrades
//! to an `"other"` item instead of aborting the whole scan.
//!
//! [`ImportedEvent::content`] carries `SessionItem::text_full` — the
//! complete extracted item text, not just the 200-char listing preview —
//! capped via [`crate::cap_content`] the same way `ng-core::Event` caps its
//! own content. `created_at` prefers each item's own `SessionItem::timestamp`
//! (real per-event time, where the harness provides one — Codex does) and
//! only falls back to the transcript file's mtime for items/harnesses that
//! don't.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use ng_sessions::{load_transcript, SessionInfo};

use crate::{cap_content, ensure_parent_dir, Result};

/// One transcript item translated into something close to `ng-core`'s
/// `Event` shape — this crate doesn't depend on `ng-core`, so the caller
/// (the daemon) is expected to map this 1:1 onto its own `Event`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedEvent {
    pub session_id: String,
    pub harness: String,
    /// Speaker role as parsed by ng-sessions (user|assistant|tool|system|other);
    /// consumers need it to map onto ng-core event kinds without guessing.
    pub role: String,
    pub kind: String,
    pub content: String,
    pub created_at: i64,
}

/// Per-file scan state, persisted at `state_path` between calls so a scan
/// only imports what changed since the last one.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileState {
    /// File mtime (seconds since epoch) as of the last scan that read it.
    mtime_epoch: u64,
    /// [CORRECTNESS-05] File size in bytes as of the last scan. mtime alone
    /// has only whole-second resolution on many filesystems, so an append
    /// that lands within the same second as a prior scan would otherwise
    /// look unchanged and get skipped forever; a file whose size moved is
    /// always treated as changed regardless of mtime. `#[serde(default)]`
    /// so a `state.json` written before this field existed still parses
    /// (as 0, which just forces one extra re-check on upgrade, not a
    /// re-import of already-seen items — `imported_item_count` still gates
    /// that).
    #[serde(default)]
    size: u64,
    /// How many items the transcript had as of the last scan; items at or
    /// beyond this index are new.
    imported_item_count: usize,
}

type ScanState = HashMap<String, FileState>;

/// Scan every `*.jsonl` file under each of `roots` (recursively) and
/// return the items that are new since the last call with the same
/// `state_path`. The harness is currently always reported as `"codex"` —
/// the only transcript format `ng-sessions` can parse today without a
/// hook — extending this to another watched-only harness is a matter of
/// tagging its roots separately once `ng-sessions` grows a parser for it.
///
/// Never fails on a per-file basis: an unreadable or unparsable transcript
/// is skipped (its prior state, if any, is left untouched so the next scan
/// retries it) rather than aborting the whole scan.
pub fn scan_transcripts(roots: &[PathBuf], state_path: &Path) -> Result<Vec<ImportedEvent>> {
    let mut state = load_state(state_path)?;
    let mut imported = Vec::new();

    let mut files = Vec::new();
    for root in roots {
        walk_jsonl(root, &mut files);
    }

    for path in files {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let mtime_epoch = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let size = metadata.len();

        let key = path.to_string_lossy().to_string();
        let prior = state.get(&key).cloned();
        if let Some(p) = &prior {
            // [CORRECTNESS-05] Both must match to skip: mtime resolution
            // can be coarser than the interval between scans, so an append
            // that happens to land in the same second as the last scan
            // must still be caught via the size change.
            if p.mtime_epoch == mtime_epoch && p.size == size {
                continue; // unchanged since last scan, nothing new
            }
        }

        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let info = SessionInfo {
            id: id.clone(),
            harness: "codex".to_string(),
            path: path.clone(),
            project: None,
            modified_at: metadata.modified().unwrap_or(UNIX_EPOCH),
            items_hint: None,
        };
        let Ok(transcript) = load_transcript(&info) else {
            continue; // leave `prior` state alone: retry next scan
        };

        // A hygiene rewrite (ngd's /api/rewrite) can shrink the transcript
        // after a scan, leaving the persisted count above the file's real
        // item count; without clamping, items appended after the shrink
        // would sit below the stale count and be skipped forever. Clamping
        // may re-import an item the rewrite renumbered — a possible
        // duplicate is preferable to silent capture loss.
        let already_imported = prior
            .as_ref()
            .map(|p| p.imported_item_count)
            .unwrap_or(0)
            .min(transcript.items.len());
        for item in transcript
            .items
            .iter()
            .filter(|i| i.index >= already_imported)
        {
            imported.push(ImportedEvent {
                session_id: id.clone(),
                harness: "codex".to_string(),
                role: item.role.clone(),
                kind: item.kind.clone(),
                content: cap_content(item.text_full.clone()),
                // [finding 06] Prefer the item's own timestamp (Codex has
                // one per event); only items/harnesses without one fall
                // back to the file's mtime at scan time.
                created_at: item.timestamp.unwrap_or(mtime_epoch as i64),
            });
        }

        state.insert(
            key,
            FileState {
                mtime_epoch,
                size,
                imported_item_count: transcript.items.len(),
            },
        );
    }

    save_state(state_path, &state)?;
    Ok(imported)
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn load_state(state_path: &Path) -> Result<ScanState> {
    if !state_path.exists() {
        return Ok(ScanState::new());
    }
    let raw = std::fs::read_to_string(state_path)?;
    // A corrupted state file must not stall future scans forever — treat
    // it as "nothing scanned yet" and let everything be reimported once,
    // rather than erroring out and blocking the daemon.
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_state(state_path: &Path, state: &ScanState) -> Result<()> {
    ensure_parent_dir(state_path)?;
    std::fs::write(state_path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    fn write_codex_transcript(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn bump_mtime(path: &Path, seconds_forward: u64) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let new_time = SystemTime::now() + Duration::from_secs(seconds_forward);
        file.set_modified(new_time).unwrap();
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    fn msg(role: &str, text: &str) -> String {
        format!(
            r#"{{"payload":{{"type":"message","role":"{role}","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
    }

    fn msg_with_timestamp(role: &str, text: &str, timestamp: &str) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","payload":{{"type":"message","role":"{role}","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn first_scan_imports_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(
            &transcript,
            &[&msg("user", "fix the parser"), &msg("assistant", "done")],
        );

        let state_path = tmp.path().join("state.json");
        let events = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].harness, "codex");
        assert!(events[0].content.contains("fix the parser"));
    }

    #[test]
    fn second_scan_with_no_changes_imports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(&transcript, &[&msg("user", "hello")]);

        let state_path = tmp.path().join("state.json");
        scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        let second = scan_transcripts(&[root], &state_path).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn second_scan_after_append_imports_only_the_increment() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(&transcript, &[&msg("user", "first message")]);

        let state_path = tmp.path().join("state.json");
        let first = scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        assert_eq!(first.len(), 1);

        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            writeln!(f, "{}", msg("assistant", "second message")).unwrap();
        }
        bump_mtime(&transcript, 5); // deterministic, avoids mtime-resolution flakiness

        let second = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(
            second.len(),
            1,
            "only the newly appended item should be reported"
        );
        assert!(second[0].content.contains("second message"));
    }

    #[test]
    fn imports_the_full_text_not_just_a_200_char_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        let long_text = "word ".repeat(100).trim_end().to_string(); // ~500 chars
        write_codex_transcript(&transcript, &[&msg("user", &long_text)]);

        let state_path = tmp.path().join("state.json");
        let events = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].content, long_text,
            "full text imported, not truncated to a preview"
        );
        assert!(events[0].content.len() > 200);
    }

    #[test]
    fn missing_root_never_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");
        let events = scan_transcripts(&[tmp.path().join("does-not-exist")], &state_path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn corrupted_state_file_does_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        write_codex_transcript(&root.join("rollout-1.jsonl"), &[&msg("user", "hi")]);
        let state_path = tmp.path().join("state.json");
        std::fs::write(&state_path, "not json").unwrap();

        let events = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(
            events.len(),
            1,
            "corrupted state treated as empty, file re-imported"
        );
    }

    // --- finding 06: real per-item created_at ---

    #[test]
    fn uses_the_item_own_timestamp_instead_of_file_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(
            &transcript,
            &[&msg_with_timestamp("user", "hi", "2026-07-18T10:00:00Z")],
        );

        let state_path = tmp.path().join("state.json");
        let events = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].created_at, 1_784_368_800,
            "must use the item's own timestamp, not file mtime"
        );
    }

    #[test]
    fn falls_back_to_file_mtime_when_item_has_no_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(&transcript, &[&msg("user", "hi")]); // no "timestamp" field

        let state_path = tmp.path().join("state.json");
        let events = scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        assert_eq!(events.len(), 1);
        let mtime_epoch = std::fs::metadata(&transcript)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(events[0].created_at, mtime_epoch);
    }

    // --- CORRECTNESS-05: size, not just mtime, drives change detection ---

    #[test]
    fn append_with_unchanged_mtime_second_is_still_imported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(&transcript, &[&msg("user", "first message")]);

        let state_path = tmp.path().join("state.json");
        let first = scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        assert_eq!(first.len(), 1);

        // Append, then force the mtime back to exactly what it was before
        // the append — reproducing "the append landed within the same
        // mtime-resolution tick as the prior scan", which mtime-only change
        // detection would miss entirely.
        let mtime_before = std::fs::metadata(&transcript).unwrap().modified().unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            writeln!(f, "{}", msg("assistant", "second message")).unwrap();
        }
        set_mtime(&transcript, mtime_before);
        assert_eq!(
            std::fs::metadata(&transcript).unwrap().modified().unwrap(),
            mtime_before,
            "sanity check: mtime truly did not change"
        );

        let second = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(
            second.len(),
            1,
            "size change must be caught even though mtime didn't move"
        );
        assert!(second[0].content.contains("second message"));
    }

    // --- finding 4: shrink via rewrite must not swallow later appends ---

    #[test]
    fn shrink_from_rewrite_does_not_swallow_later_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(
            &transcript,
            &[
                &msg("user", "one"),
                &msg("assistant", "two"),
                &msg("user", "three"),
            ],
        );

        let state_path = tmp.path().join("state.json");
        let first = scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        assert_eq!(first.len(), 3);

        // A hygiene rewrite drops two lines: the file now has fewer items
        // than the persisted imported_item_count.
        write_codex_transcript(&transcript, &[&msg("user", "one")]);
        bump_mtime(&transcript, 5);
        let after_shrink = scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        assert!(
            after_shrink.is_empty(),
            "the clamp alone must not re-import the surviving items"
        );

        // Append after the shrink: with the stale count (3) still in state,
        // this item at index 1 would be skipped silently forever.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            writeln!(f, "{}", msg("assistant", "four")).unwrap();
        }
        bump_mtime(&transcript, 10);
        let after_append = scan_transcripts(&[root], &state_path).unwrap();
        assert_eq!(
            after_append.len(),
            1,
            "the item appended after the shrink must be imported"
        );
        assert!(after_append[0].content.contains("four"));
    }

    #[test]
    fn unchanged_file_with_identical_mtime_and_size_is_still_skipped() {
        // Guards against the size check making the scan over-eager: a
        // truly untouched file must still be skipped, not reimported every
        // time just because size-equality alone isn't gated correctly.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("codex-sessions");
        let transcript = root.join("rollout-1.jsonl");
        write_codex_transcript(&transcript, &[&msg("user", "hello")]);

        let state_path = tmp.path().join("state.json");
        scan_transcripts(std::slice::from_ref(&root), &state_path).unwrap();
        let second = scan_transcripts(&[root], &state_path).unwrap();
        assert!(second.is_empty());
    }
}
