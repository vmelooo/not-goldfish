//! ng-adapters: pure, testable harness-integration logic for not-goldfish.
//!
//! Every function here takes its base path(s) as a parameter — no `$HOME`
//! or other environment lookups inside the logic itself — so tests run
//! against a tempdir and production code (wired up by `ng-cli`, not this
//! crate) passes the real paths. Every config write follows the same
//! shape: read what's there (defaulting to an empty document, never
//! failing just because the file doesn't exist yet), back it up if it
//! existed, merge the new content in without discarding unrelated keys,
//! create parent directories as needed, then write.

pub mod dispatch;
pub mod hooks;
pub mod mcp;
pub mod personas;
pub mod saver_cli;
pub mod saver_mcp;
pub mod savers;
pub mod watcher;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("toml parse error: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Copy `path` to `path` with a `.ng-backup` suffix appended to its file
/// name (e.g. `settings.json` -> `settings.json.ng-backup`), if `path`
/// exists. Returns `None` when there was nothing to back up (fresh
/// install), `Some(backup_path)` otherwise. Existing backups are
/// overwritten — this is a pre-write safety net, not a version history.
pub(crate) fn backup_if_exists(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut backup_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    backup_name.push(".ng-backup");
    let backup_path = path.with_file_name(backup_name);
    std::fs::copy(path, &backup_path)?;
    Ok(Some(backup_path))
}

pub(crate) fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Monotonic tiebreaker for temp file names, alongside the nanosecond
/// timestamp and pid — guards against two writes landing in the same
/// process within the same clock tick.
static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// [finding 07] Write `bytes` to `path` without ever leaving it truncated
/// or half-written: the content lands in a sibling temp file in the same
/// directory first (fsynced before the swap), then `fs::rename` moves it
/// into place atomically. A crash before the rename leaves `path` exactly
/// as it was; a crash after leaves the new content — never a partial file.
///
/// Every `std::fs::write` in `hooks.rs`, `mcp.rs`, and `personas.rs` that
/// touches a harness's own config file (`settings.json`, `config.toml`,
/// persona `.md` files) routes through this instead of writing in place,
/// because those are files the user's harness reads on every launch — a
/// truncated one breaks the harness, not just not-goldfish.
///
/// Mirrors `ng_sessions::rewrite::rewrite_jsonl`'s durability approach;
/// re-implemented rather than shared since this crate has no dependency on
/// `ng-sessions`.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_parent_dir(path)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{file_name}.ng-tmp-{nanos}-{}-{seq}",
        std::process::id()
    ));

    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp_path, path)?;
    if let Ok(dir_handle) = std::fs::File::open(dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

/// [finding 15] Mirrors `ng_core::event::MAX_CONTENT_BYTES` (256 KiB).
/// Copied rather than imported when this crate had no `ng-core` dependency;
/// the dependency has since arrived (for the `Saver` trait, in
/// `saver_cli.rs`), but the constant stays local: the cap is a shared
/// *convention* between the two crates' storage, not a type either owns.
/// This is the one copy of that convention within `ng-adapters` — every
/// module that needs to cap content before handing it to the daemon (today
/// just `watcher.rs`) calls [`cap_content`] here rather than
/// re-implementing its own truncation.
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// Truncate `text` to [`MAX_CONTENT_BYTES`], on a char boundary, appending
/// an explicit marker when data was cut — same shape as
/// `ng_core::Event::cap_content`, so an imported event never silently
/// looks complete when it was chopped.
pub(crate) fn cap_content(text: String) -> String {
    if text.len() <= MAX_CONTENT_BYTES {
        return text;
    }
    let mut cut = MAX_CONTENT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let total = text.len();
    let mut capped = text;
    capped.truncate(cut);
    capped.push_str(&format!("\n[ng: truncated, original {total} bytes]"));
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_if_exists_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.json");
        assert!(backup_if_exists(&path).unwrap().is_none());
    }

    #[test]
    fn backup_if_exists_copies_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"a":1}"#).unwrap();
        let backup = backup_if_exists(&path).unwrap().unwrap();
        assert_eq!(
            backup.file_name().unwrap().to_str().unwrap(),
            "settings.json.ng-backup"
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn atomic_write_creates_file_and_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/config.json");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn atomic_write_replaces_existing_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file_behind_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        atomic_write(&path, b"content").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("ng-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp file must be renamed away, not left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_leaves_original_intact_when_tmp_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "original").unwrap();

        // Make the directory read-only so creating the sibling tmp file
        // fails partway through — this must never touch `path` itself.
        let mut perms = std::fs::metadata(tmp.path()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(tmp.path(), perms).unwrap();

        let result = atomic_write(&path, b"new content");

        // Restore write access so the tempdir can clean itself up.
        let mut restore = std::fs::metadata(tmp.path()).unwrap().permissions();
        restore.set_mode(0o700);
        std::fs::set_permissions(tmp.path(), restore).unwrap();

        if result.is_ok() {
            // Running as root (or a filesystem that ignores mode bits): the
            // permission trick this test relies on doesn't apply here.
            return;
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn cap_content_truncates_on_char_boundary_with_marker() {
        let long = "é".repeat(MAX_CONTENT_BYTES); // 2 bytes/char, well over the cap
        let capped = cap_content(long.clone());
        assert!(capped.len() < long.len());
        assert!(capped.contains("[ng: truncated, original"));
        assert!(capped.is_char_boundary(capped.find('[').unwrap()));
    }

    #[test]
    fn cap_content_leaves_short_text_untouched() {
        let short = "hello world".to_string();
        assert_eq!(cap_content(short.clone()), short);
    }
}
