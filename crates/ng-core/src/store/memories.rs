//! "Memória" view operations: list, soft-hide/restore, annotate, manual add.

use rusqlite::params;

use super::util::now_epoch;
use super::{Memory, Store};
use crate::event::Event;
use crate::Result;

impl Store {
    /// List stored memories for the "Memória" view, newest first. `project`
    /// restricts to one project path (`None` = every project). With
    /// `include_hidden = false`, hidden rows are omitted (mirroring what
    /// search/injection see); with `true`, hidden rows are included and
    /// flagged so the UI can offer to restore them. Structural session
    /// markers are always skipped — they carry no memory content.
    pub fn list_memories(
        &self,
        project: Option<&str>,
        include_hidden: bool,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let mut where_clauses = vec!["kind NOT IN ('session_start', 'session_end')".to_string()];
        if !include_hidden {
            where_clauses.push("hidden_at IS NULL".to_string());
        }
        if project.is_some() {
            where_clauses.push("project = ?1".to_string());
        }
        let sql = format!(
            "SELECT id, project, harness, kind, content, tags, tokens_est,
                    created_at, hidden_at, note, manual
             FROM events
             WHERE {}
             ORDER BY created_at DESC, id DESC
             LIMIT ?2",
            where_clauses.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Memory> {
            let hidden_at: Option<i64> = row.get(8)?;
            let manual: i64 = row.get(10)?;
            Ok(Memory {
                id: row.get(0)?,
                project: row.get(1)?,
                harness: row.get(2)?,
                kind: row.get(3)?,
                content: row.get(4)?,
                tags: row.get(5)?,
                tokens_est: row.get(6)?,
                created_at: row.get(7)?,
                hidden: hidden_at.is_some(),
                note: row.get(9)?,
                manual: manual != 0,
            })
        };
        // ?1 (project) is only referenced by the SQL when `project.is_some()`,
        // but binding an unused parameter is harmless — always pass both so the
        // positional ?2 (limit) keeps its index regardless of the filter.
        let rows = stmt.query_map(params![project.unwrap_or(""), limit as i64], map)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Soft-hide a memory: stamp `hidden_at` so it leaves search/injection
    /// without touching its content, FTS postings, or embedding. Idempotent
    /// (re-hiding an already-hidden row leaves the original stamp). Returns
    /// whether a row was affected. NEVER deletes.
    pub fn hide_memory(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE events SET hidden_at = ?1 WHERE id = ?2 AND hidden_at IS NULL",
            params![now_epoch(), id],
        )?;
        Ok(n > 0)
    }

    /// Restore a hidden memory: clear `hidden_at` so it re-enters
    /// search/injection. Returns whether a row was affected.
    pub fn unhide_memory(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE events SET hidden_at = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(n > 0)
    }

    /// Attach (or replace) a user note on a memory. An empty/whitespace note
    /// clears the annotation (stored as NULL). Returns whether a row was
    /// affected. The note lives in its own column and never alters the
    /// captured content or its FTS index.
    pub fn annotate_memory(&self, id: i64, note: &str) -> Result<bool> {
        let trimmed = note.trim();
        let value: Option<&str> = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        let n = self.conn.execute(
            "UPDATE events SET note = ?1 WHERE id = ?2",
            params![value, id],
        )?;
        Ok(n > 0)
    }

    /// Insert a memory the user added by hand. Stored as a normal event
    /// (`harness = 'manual'`, `kind = 'manual'`, `manual = 1`) so it flows
    /// through the same FTS trigger and becomes searchable/injectable like any
    /// captured memory. Returns the new row id.
    pub fn add_manual_memory(&self, project: &str, content: &str, tags: &str) -> Result<i64> {
        let event = Event {
            session_id: "manual".to_string(),
            project: project.to_string(),
            harness: "manual".to_string(),
            kind: "manual".to_string(),
            content: content.to_string(),
            tags: tags.to_string(),
            meta: None,
            created_at: now_epoch(),
        }
        .cap_content();
        self.conn.execute(
            "INSERT INTO events
                (session_id, project, harness, kind, content, tags, tokens_est, created_at, manual)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
            params![
                event.session_id,
                event.project,
                event.harness,
                event.kind,
                event.content,
                event.tags,
                event.tokens_est(),
                event.created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Edit the CONTENT (and tags) of a *manual* memory in place. Captured
    /// memories are never editable — this returns `Ok(false)` for any row with
    /// `manual = 0` (or a missing id), leaving it untouched, honoring the
    /// "nothing captured is deleted/mutated" invariant. For a manual row it
    /// updates content + tags, recomputes `tokens_est`, and drops the stale
    /// embedding row so the background worker re-embeds the new content. The
    /// `events_au` AFTER UPDATE trigger keeps the FTS index in sync
    /// automatically. Returns whether a manual row was edited.
    pub fn edit_memory_content(&self, id: i64, content: &str, tags: &str) -> Result<bool> {
        // Recompute token estimate from the new content via the same Event
        // path used on insert, so the stored tokens_est stays consistent.
        let event = Event {
            session_id: "manual".to_string(),
            project: String::new(),
            harness: "manual".to_string(),
            kind: "manual".to_string(),
            content: content.to_string(),
            tags: tags.to_string(),
            meta: None,
            created_at: 0,
        }
        .cap_content();
        let n = self.conn.execute(
            "UPDATE events SET content = ?1, tags = ?2, tokens_est = ?3
             WHERE id = ?4 AND manual = 1",
            params![event.content, event.tags, event.tokens_est(), id],
        )?;
        if n > 0 {
            // Stale embedding: drop it so events_without_embedding picks the
            // row up again and the enrich worker recomputes the vector.
            self.conn
                .execute("DELETE FROM embeddings WHERE event_id = ?1", params![id])?;
        }
        Ok(n > 0)
    }
}
