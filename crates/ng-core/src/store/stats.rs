//! Event insertion and aggregate statistics over `events`.

use rusqlite::params;

use super::Store;
use crate::event::Event;
use crate::Result;

impl Store {
    /// Insert one captured event. Returns its row id.
    pub fn insert_event(&self, event: &Event) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (session_id, project, harness, kind, content, tags, tokens_est, created_at, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                event.session_id,
                event.project,
                event.harness,
                event.kind,
                event.content,
                event.tags,
                event.tokens_est(),
                event.created_at,
                event.meta,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// (event count, distinct sessions, total estimated tokens stored)
    pub fn stats(&self) -> Result<(i64, i64, i64)> {
        let row = self.conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT session_id), COALESCE(SUM(tokens_est), 0) FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(row)
    }

    /// [`Store::stats`] com escopo: `(events, sessions, tokens_est,
    /// MIN(created_at))` sobre `events`, opcionalmente restrito a um
    /// `project` e/ou a `created_at >= since`. O `MIN` é o "usando desde"
    /// do `ng gain`; `None` quando não há eventos no escopo.
    pub fn stats_scoped(
        &self,
        project: Option<&str>,
        since: Option<i64>,
    ) -> Result<(i64, i64, i64, Option<i64>)> {
        let row = self.conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT session_id), COALESCE(SUM(tokens_est), 0),
                    MIN(created_at)
             FROM events
             WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR created_at >= ?2)",
            params![project, since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        Ok(row)
    }
}
