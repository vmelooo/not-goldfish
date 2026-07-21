//! Retry + dead-letter handling for the daemon's single writer thread (see
//! `main.rs`'s `writer_loop`). Split into its own module so the pure
//! retry-counting core and the dead-letter file format are testable via
//! `crates/ngd/tests/` without spinning up the real socket/channel plumbing.
//!
//! By the time an event reaches the writer, the hook that captured it has
//! already exited believing capture succeeded — a single failed
//! `INSERT` (transient lock contention, a momentarily full disk, ...) must
//! not just vanish behind an `eprintln!`. Every attempt that exhausts
//! retries is appended to a dead-letter file instead, so the event stays
//! recoverable.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use ng_core::{Event, Store};

pub const INSERT_RETRY_ATTEMPTS: u32 = 3;
pub const INSERT_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Insert `event` into `store`, retrying up to [`INSERT_RETRY_ATTEMPTS`]
/// times on failure. WAL + `busy_timeout` already retry transient lock
/// contention at the SQLite driver level; this covers a contention burst
/// long enough to still exceed that window (e.g. a storm of parallel
/// sessions hitting the daemon at once).
pub fn insert_with_retry(store: &Store, event: &Event) -> Result<i64, String> {
    insert_with_retry_using(
        || store.insert_event(event).map_err(|e| e.to_string()),
        INSERT_RETRY_ATTEMPTS,
        INSERT_RETRY_DELAY,
    )
}

/// Generic retry core, independent of [`Store`] so it's unit-testable with
/// a fake failing closure instead of needing to force a real SQLite
/// failure (not practical to do from outside `ng-core`, which this crate
/// must not modify).
pub fn insert_with_retry_using<F>(
    mut insert: F,
    attempts: u32,
    delay: Duration,
) -> Result<i64, String>
where
    F: FnMut() -> Result<i64, String>,
{
    let mut last_err = None;
    for attempt in 0..attempts.max(1) {
        match insert() {
            Ok(id) => return Ok(id),
            Err(err) => {
                if attempt + 1 < attempts {
                    std::thread::sleep(delay);
                }
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "no attempts made".to_string()))
}

/// Appends `event` as one JSON line to the dead-letter file at `path`,
/// creating it if needed. Never truncates or rewrites — every event that
/// exhausts retries must land here in arrival order, alongside whatever
/// was already dead-lettered before it, not overwrite it.
pub fn append_dead_letter(path: &Path, event: &Event) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> Event {
        Event {
            session_id: "s1".to_string(),
            project: "/tmp/proj".to_string(),
            harness: "claude-code".to_string(),
            kind: "prompt".to_string(),
            content: "conteudo de teste".to_string(),
            tags: String::new(),
            meta: None,
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn succeeding_insert_does_not_retry() {
        let mut attempts = 0;
        let result = insert_with_retry_using(
            || {
                attempts += 1;
                Ok(42)
            },
            3,
            Duration::from_millis(0),
        );
        assert_eq!(result, Ok(42));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn failing_insert_retries_exactly_configured_attempts_then_errors() {
        let mut attempts = 0;
        let result = insert_with_retry_using(
            || {
                attempts += 1;
                Err("simulated failure".to_string())
            },
            3,
            Duration::from_millis(0),
        );
        assert!(result.is_err());
        assert_eq!(attempts, 3);
    }

    #[test]
    fn transient_failure_then_success_recovers_without_exhausting_retries() {
        let mut attempts = 0;
        let result = insert_with_retry_using(
            || {
                attempts += 1;
                if attempts < 2 {
                    Err("transient".to_string())
                } else {
                    Ok(7)
                }
            },
            3,
            Duration::from_millis(0),
        );
        assert_eq!(result, Ok(7));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn exhausted_retries_land_the_event_in_the_dead_letter_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dead_letter_path = tmp.path().join("dead-letter.jsonl");
        let event = sample_event();

        let result: Result<i64, String> = insert_with_retry_using(
            || Err("db is locked".to_string()),
            3,
            Duration::from_millis(0),
        );
        assert!(result.is_err());
        append_dead_letter(&dead_letter_path, &event).unwrap();

        let content = std::fs::read_to_string(&dead_letter_path).unwrap();
        let parsed: Event = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed.session_id, event.session_id);
        assert_eq!(parsed.content, event.content);
    }

    #[test]
    fn dead_letter_appends_without_truncating_prior_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dead-letter.jsonl");
        let mut e2 = sample_event();
        e2.session_id = "s2".to_string();

        append_dead_letter(&path, &sample_event()).unwrap();
        append_dead_letter(&path, &e2).unwrap();

        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        let first: Event = serde_json::from_str(&lines[0]).unwrap();
        let second: Event = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(first.session_id, "s1");
        assert_eq!(second.session_id, "s2");
    }

    #[test]
    fn dead_letter_creates_parent_free_file_on_first_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dead-letter.jsonl");
        assert!(!path.exists());
        append_dead_letter(&path, &sample_event()).unwrap();
        assert!(path.exists());
    }
}
