//! Safe, atomic rewriting of JSONL transcripts for procedural hygiene.
//!
//! Only line-addressable formats (JSONL: claude/codex/kimi) can be
//! rewritten — this is why [`crate::model::SessionItem::raw_line`] is an
//! `Option`. The flow is validate-then-swap: every replacement must already
//! be valid JSON before anything touches disk, a full backup is written
//! first, and the new content lands via `rename` in the same directory so a
//! crash mid-write can never corrupt the original file. Line order is never
//! changed — lines are only dropped or replaced in place.
//!
//! Durability: the backup and the new content are both written to disk with
//! an explicit `sync_all()` before the rename, and the parent directory is
//! synced after the rename too. Without that last sync, a crash right after
//! `rename()` can — on some filesystems — leave the directory entry
//! pointing at the old inode again once the journal replays, silently
//! reverting a hygiene pass whose backup and log already claimed it
//! happened.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

/// Split file content into physical lines with the *exact* count and order
/// `str::lines()` produces (which is what every parser's `SessionItem::raw_line`
/// numbering is built on: `content.lines().enumerate()`), so a 1-based line
/// number computed at parse time always addresses the same line here.
///
/// Unlike `str::lines()`, a line's trailing `\r` (CRLF files) is kept
/// attached to that line's slice rather than stripped — `rewrite_jsonl`
/// needs that to reproduce the original bytes exactly for every line it
/// doesn't touch. Concretely: exactly one trailing `\n` is stripped from
/// the whole content first (whether the file ends in a newline is tracked
/// separately, not treated as an extra empty line), then the remainder is
/// split on `\n`; any `\r` immediately before a `\n` stays part of the
/// preceding line. Empty content yields zero lines.
///
/// This is the one canonical line-splitting rule for rewriting a
/// transcript; `ngd`'s `/api/rewrite` handler (via `ngd`'s stub module)
/// uses this same function, so the two can never disagree about which line
/// is which.
pub fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let body = content.strip_suffix('\n').unwrap_or(content);
    body.split('\n').collect()
}

/// Monotonic tiebreaker for backup/tmp file names: nanosecond timestamps
/// can still collide under clock coarsening or back-to-back calls within
/// the same tick, so uniqueness is guaranteed by this counter, not by the
/// clock alone.
static REWRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name fragment that is unique per call within this process (and, via
/// the pid, across processes too) — used for both the backup and the tmp
/// file, so two rewrites of the same file in the same second never collide
/// and clobber each other's backup.
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = REWRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{}-{seq}", std::process::id())
}

/// Write `contents` to `path` and fsync it before returning, so the caller
/// can rely on the bytes actually being on disk (not just in the page
/// cache) once this returns `Ok`.
fn write_durably(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

/// Remove the lines in `drops` and/or replace the lines in `replacements`
/// (both use the 1-based line numbers found in
/// [`crate::model::SessionItem::raw_line`]) in the JSONL file at `path`.
/// Returns the path of the backup written before the swap. On any failure
/// (invalid replacement JSON, I/O error) the original file is left
/// untouched.
///
/// The transcript may be *live*: the harness can append lines while this
/// runs. Lines appended after the initial snapshot are detected by a
/// re-read right before the swap and carried over verbatim into the new
/// content (drops/replacements only ever address snapshot lines); any
/// non-append concurrent change aborts with the file untouched. A tiny
/// window between that re-read and the `rename` remains — without file
/// locking it cannot be closed, only shrunk to microseconds.
pub fn rewrite_jsonl(
    path: &Path,
    drops: &[usize],
    replacements: &[(usize, String)],
) -> Result<PathBuf> {
    rewrite_jsonl_hooked(path, drops, replacements, || {})
}

/// Real implementation, with a test seam: `after_snapshot` runs right after
/// the initial read — exactly the window where a live harness can append
/// new lines before the swap — so tests can exercise the race
/// deterministically.
fn rewrite_jsonl_hooked(
    path: &Path,
    drops: &[usize],
    replacements: &[(usize, String)],
    after_snapshot: impl FnOnce(),
) -> Result<PathBuf> {
    for (line_no, content) in replacements {
        // A replacement with an embedded line break would become several
        // physical lines once joined by '\n', shifting every later
        // `raw_line` address — reject it outright (pretty-printed JSON is
        // still valid JSON, so the parse below wouldn't catch this).
        if content.contains('\n') || content.contains('\r') {
            return Err(Error::Other(format!(
                "replacement for line {line_no} contains a line break — must be a single line of compact JSON"
            )));
        }
        serde_json::from_str::<serde_json::Value>(content).map_err(|e| {
            Error::Other(format!(
                "replacement for line {line_no} is not valid JSON: {e}"
            ))
        })?;
    }

    let original = fs::read_to_string(path)?;
    after_snapshot();
    let had_trailing_newline = original.ends_with('\n');
    let lines = split_lines(&original);

    let drop_set: HashSet<usize> = drops.iter().copied().collect();
    let replace_map: HashMap<usize, &String> = replacements.iter().map(|(n, c)| (*n, c)).collect();

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        if drop_set.contains(&line_no) {
            continue;
        }
        match replace_map.get(&line_no) {
            Some(replacement) => out.push((*replacement).clone()),
            None => out.push((*line).to_string()),
        }
    }

    let mut new_content = out.join("\n");
    if had_trailing_newline && !out.is_empty() {
        new_content.push('\n');
    }

    // The harness may have appended to the live transcript since the
    // snapshot read above; a blind rename would overwrite (and lose) those
    // lines — they'd be in neither the backup nor the new content. Re-read
    // and reconcile: identical content proceeds as-is; a pure append (the
    // snapshot is a prefix ending at a line boundary) has its tail carried
    // over verbatim; anything else aborts with the file untouched.
    let current = fs::read_to_string(path)?;
    let mut backup_source = original;
    if current != backup_source {
        let appendable = had_trailing_newline || backup_source.is_empty();
        if !appendable || !current.starts_with(backup_source.as_str()) {
            return Err(Error::Other(format!(
                "{} changed under rewrite in a non-append way — aborting, file untouched",
                path.display()
            )));
        }
        // `new_content` ends at a line boundary here (trailing '\n' kept
        // when the snapshot had one, or empty when everything was dropped),
        // so the appended tail splices in without disturbing line math.
        new_content.push_str(&current[backup_source.len()..]);
        // The backup must capture the file as it is being replaced,
        // appended tail included.
        backup_source = current;
    }

    let suffix = unique_suffix();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session");
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let backup_path = dir.join(format!("{file_name}.{suffix}.ng-bak"));
    write_durably(&backup_path, backup_source.as_bytes())?;

    let tmp_path = dir.join(format!(".{file_name}.ng-tmp-{suffix}"));
    write_durably(&tmp_path, new_content.as_bytes())?;
    fs::rename(&tmp_path, path)?;

    // Best-effort: fsync the parent directory so the rename itself survives
    // a crash. If this fails (e.g. platform doesn't allow opening a
    // directory as a File) the rewrite already succeeded from the caller's
    // point of view — this is extra durability, not correctness of the
    // in-memory result.
    if let Ok(dir_handle) = fs::File::open(dir) {
        let _ = dir_handle.sync_all();
    }

    Ok(backup_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn noop_rewrite_is_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);
        let before = fs::read(&path).unwrap();
        rewrite_jsonl(&path, &[], &[]).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn drop_removes_exactly_that_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(
            tmp.path(),
            "s.jsonl",
            &[r#"{"a":1}"#, r#"{"a":2}"#, r#"{"a":3}"#],
        );
        rewrite_jsonl(&path, &[2], &[]).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, "{\"a\":1}\n{\"a\":3}\n");
    }

    #[test]
    fn backup_matches_original() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#]);
        let before = fs::read(&path).unwrap();
        let backup = rewrite_jsonl(&path, &[1], &[]).unwrap();
        let backup_bytes = fs::read(&backup).unwrap();
        assert_eq!(before, backup_bytes);
    }

    #[test]
    fn invalid_replacement_json_leaves_original_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);
        let before = fs::read(&path).unwrap();
        let err = rewrite_jsonl(&path, &[], &[(1, "not json".to_string())]);
        assert!(err.is_err());
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn replacement_swaps_line_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);
        rewrite_jsonl(&path, &[], &[(2, r#"{"a":99}"#.to_string())]).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, "{\"a\":1}\n{\"a\":99}\n");
    }

    // --- finding 13 / CORRECTNESS-01: unique backups, no same-second collision ---

    #[test]
    fn two_rewrites_in_the_same_second_produce_two_distinct_backups() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);

        // Both calls happen back to back, almost certainly within the same
        // wall-clock second — the old epoch-seconds backup name would
        // collide here and silently overwrite the first backup.
        let backup1 = rewrite_jsonl(&path, &[], &[(1, r#"{"a":11}"#.to_string())]).unwrap();
        let backup2 = rewrite_jsonl(&path, &[], &[(2, r#"{"a":22}"#.to_string())]).unwrap();

        assert_ne!(
            backup1, backup2,
            "each rewrite must get its own backup file"
        );
        assert!(
            backup1.exists(),
            "first backup must survive the second rewrite"
        );
        assert!(backup2.exists());
        assert_eq!(
            fs::read_to_string(&backup1).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );
        assert_eq!(
            fs::read_to_string(&backup2).unwrap(),
            "{\"a\":11}\n{\"a\":2}\n"
        );
    }

    #[test]
    fn noop_rewrite_is_still_byte_identical_after_durability_changes() {
        // Guards against a regression in the fsync/rename plumbing: the
        // observable result (bytes on disk) must be unchanged even though
        // the write path now goes through File::create + sync_all instead
        // of fs::write.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(
            tmp.path(),
            "s.jsonl",
            &[r#"{"a":1}"#, r#"{"a":2}"#, r#"{"a":3}"#],
        );
        let before = fs::read(&path).unwrap();
        rewrite_jsonl(&path, &[], &[]).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after);
    }

    // --- CORRECTNESS-03 part a / finding 19: canonical split_lines ---

    #[test]
    fn split_lines_matches_str_lines_count_and_order() {
        let content = "a\nb\nc\n";
        let canonical: Vec<&str> = split_lines(content)
            .iter()
            .map(|l| l.trim_end_matches('\r'))
            .collect();
        let std_lines: Vec<&str> = content.lines().collect();
        assert_eq!(canonical, std_lines);
    }

    #[test]
    fn split_lines_preserves_crlf_and_handles_no_trailing_newline() {
        let content = "a\r\nb\r\nc";
        let lines = split_lines(content);
        assert_eq!(
            lines,
            vec!["a\r", "b\r", "c"],
            "CRLF kept on the line, last line has no terminator at all"
        );
        // Rejoining with '\n' must reproduce the original bytes exactly.
        assert_eq!(lines.join("\n"), content);
    }

    #[test]
    fn split_lines_on_empty_content_is_empty() {
        assert!(split_lines("").is_empty());
    }

    // --- finding 1: lines appended between snapshot and rename survive ---

    #[test]
    fn append_between_snapshot_and_rename_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);
        // The hook fires in the exact window where the harness would append
        // to the live transcript: after the snapshot read, before the swap.
        let appended = r#"{"a":3}"#;
        let backup = rewrite_jsonl_hooked(&path, &[1], &[], || {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{appended}").unwrap();
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"a\":2}\n{\"a\":3}\n",
            "drop applied to the snapshot line, appended line kept"
        );
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n",
            "backup captures the file as it was being replaced, tail included"
        );
    }

    #[test]
    fn non_append_concurrent_change_aborts_without_touching_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);
        let err = rewrite_jsonl_hooked(&path, &[1], &[], || {
            fs::write(&path, "{\"rewritten\":true}\n").unwrap();
        });
        assert!(err.is_err(), "a non-append concurrent change must abort");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"rewritten\":true}\n",
            "the concurrent writer's content must be left untouched"
        );
    }

    // --- finding 2: multi-line replacements would shift 1-based addressing ---

    #[test]
    fn multiline_replacement_is_rejected_and_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "s.jsonl", &[r#"{"a":1}"#, r#"{"a":2}"#]);
        let before = fs::read(&path).unwrap();

        // Pretty-printed JSON is valid JSON, so serde alone wouldn't reject
        // it — but spliced raw into the '\n' join it becomes three physical
        // lines and shifts every later raw_line address.
        let pretty = "{\n  \"a\": 9\n}".to_string();
        let err = rewrite_jsonl(&path, &[], &[(1, pretty)]);
        assert!(err.is_err());
        assert!(
            format!("{}", err.unwrap_err()).contains("line break"),
            "error must say why the replacement was rejected"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "file left untouched");

        let with_cr = "{\"a\": 9}\r".to_string();
        assert!(rewrite_jsonl(&path, &[], &[(1, with_cr)]).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rewrite_round_trips_crlf_file_without_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crlf.jsonl");
        fs::write(&path, "{\"a\":1}\r\n{\"a\":2}\r\n{\"a\":3}").unwrap();
        let before = fs::read(&path).unwrap();

        rewrite_jsonl(&path, &[], &[]).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "noop rewrite preserves CRLF endings byte-for-byte"
        );

        rewrite_jsonl(&path, &[], &[(2, r#"{"a":99}"#.to_string())]).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        // Line 1 and 3 are untouched, so they keep their original CRLF; the
        // replacement is caller-supplied JSON text with no \r of its own —
        // preserving CRLF is only a promise for lines that weren't rewritten.
        assert_eq!(
            after, "{\"a\":1}\r\n{\"a\":99}\n{\"a\":3}",
            "replacement lands on the correct (2nd) line"
        );
    }
}
