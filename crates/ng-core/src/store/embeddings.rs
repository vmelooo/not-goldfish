//! Embedding storage: upsert, backlog listing and vec (de)serialization.

use rusqlite::params;

use super::Store;
use crate::Result;

impl Store {
    /// Store (or replace) the embedding for one event under `model`.
    pub fn upsert_embedding(&self, event_id: i64, model: &str, vec: &[f32]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embeddings (event_id, model, vec) VALUES (?1, ?2, ?3)
             ON CONFLICT(event_id) DO UPDATE SET model = excluded.model, vec = excluded.vec",
            params![event_id, model, encode_vec(vec)],
        )?;
        Ok(())
    }

    /// (id, content) of up to `limit` searchable events that have no
    /// embedding stored yet for `model` — the enrichment worker's backlog.
    pub fn events_without_embedding(
        &self,
        model: &str,
        limit: usize,
    ) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.content FROM events e
             LEFT JOIN embeddings em ON em.event_id = e.id AND em.model = ?1
             WHERE e.kind IN ('prompt', 'tool_output', 'assistant')
               AND em.event_id IS NULL
             ORDER BY e.id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![model, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Store multiple embeddings in one transaction instead of one fsync'd
    /// statement per row — the enrichment worker calls this once per
    /// `BATCH_SIZE`-sized backlog batch instead of looping `upsert_embedding`.
    pub fn upsert_embeddings_batch(&self, model: &str, items: &[(i64, Vec<f32>)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO embeddings (event_id, model, vec) VALUES (?1, ?2, ?3)
                 ON CONFLICT(event_id) DO UPDATE SET model = excluded.model, vec = excluded.vec",
            )?;
            for (event_id, vec) in items {
                stmt.execute(params![event_id, model, encode_vec(vec)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// Little-endian f32 encoding for the `embeddings.vec` BLOB column.
pub(super) fn encode_vec(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Inverse of [`encode_vec`]. Any trailing bytes that don't form a full
/// f32 (corrupt row) are silently dropped rather than panicking.
pub(super) fn decode_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
