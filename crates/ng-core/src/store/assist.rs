//! Cursors and queries backing the daemon-side assistant import
//! (transcript parse on session_end).

use rusqlite::params;

use super::util::now_epoch;
use super::Store;
use crate::Result;

/// One session_end event whose transcript still needs an assistant sweep.
#[derive(Debug, Clone)]
pub struct PendingImport {
    pub event_id: i64,
    pub session_id: String,
    pub project: String,
    pub harness: String,
    pub transcript_path: String,
}

/// Result of one pending-import scan: the importable rows plus the highest
/// events.id the scan actually walked — including rows the meta filter
/// skipped. The caller advances the cursor over `max_scanned_id`, so a
/// LIMIT window made only of unparseable metas can never wedge the import.
#[derive(Debug, Clone, Default)]
pub struct PendingScan {
    pub imports: Vec<PendingImport>,
    /// Highest events.id examined by this scan; 0 when no row matched the
    /// SQL window at all.
    pub max_scanned_id: i64,
}

impl Store {
    pub fn assist_cursor_get(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT last_event FROM assist_cursor WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    pub fn assist_cursor_set(&self, last_event: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE assist_cursor SET last_event = ?1 WHERE id = 1",
            params![last_event],
        )?;
        Ok(())
    }

    pub fn transcript_imported_count(&self, session_id: &str) -> Result<usize> {
        let count: Option<i64> = self
            .conn
            .query_row(
                "SELECT imported_items FROM transcript_cursor WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .ok();
        Ok(count.unwrap_or(0) as usize)
    }

    pub fn set_transcript_imported_count(&self, session_id: &str, count: usize) -> Result<()> {
        self.conn.execute(
            "INSERT INTO transcript_cursor (session_id, imported_items, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET imported_items = ?2, updated_at = ?3",
            params![session_id, count as i64, now_epoch()],
        )?;
        Ok(())
    }

    /// session_end events newer than the assist cursor that carry a
    /// transcript_path in their meta. Both the Stop and SessionEnd hooks
    /// record this same kind, so Stop's repeated firings drive incremental
    /// imports. JSON extraction happens in Rust (not json_extract in SQL)
    /// so a malformed meta row is skipped, never fatal — but still counted
    /// in [`PendingScan::max_scanned_id`], so the cursor moves past it.
    pub fn pending_transcript_imports(&self, limit: usize) -> Result<PendingScan> {
        let cursor = self.assist_cursor_get()?;
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, project, harness, meta
             FROM events
             WHERE id > ?1 AND kind = 'session_end' AND meta IS NOT NULL
             ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cursor, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut scan = PendingScan::default();
        for row in rows {
            let (event_id, session_id, project, harness, meta) = row?;
            scan.max_scanned_id = scan.max_scanned_id.max(event_id);
            let Some(path) = meta
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| {
                    v.get("transcript_path")
                        .and_then(|p| p.as_str())
                        .map(String::from)
                })
            else {
                continue;
            };
            scan.imports.push(PendingImport {
                event_id,
                session_id,
                project,
                harness,
                transcript_path: path,
            });
        }
        Ok(scan)
    }
}
