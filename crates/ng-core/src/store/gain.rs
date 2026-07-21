//! `gain_ledger` operations: insert records and summarize by kind.

use rusqlite::params;

use super::Store;
use crate::gain::GainRecord;
use crate::Result;

impl Store {
    /// Insert one [`GainRecord`] into `gain_ledger`. Single prepared-and-run
    /// INSERT — chamado no caminho de escrita do daemon e nos rewrites de
    /// higiene, sempre *depois* do efeito real (injeção emitida / rename
    /// atômico bem-sucedido).
    pub fn insert_gain(&self, record: &GainRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO gain_ledger (kind, session_id, project, tokens, items, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.kind,
                record.session_id,
                record.project,
                record.tokens,
                record.items,
                record.created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Agregado do `gain_ledger` por kind: `(kind, passadas, itens, tokens)`,
    /// mesmo escopo opcional de [`Store::stats_scoped`]. Num banco antigo
    /// aberto read-only a tabela pode ainda não existir (ela só nasce num
    /// open read-write) — isso é "sem dados", não um erro.
    pub fn gain_summary(
        &self,
        project: Option<&str>,
        since: Option<i64>,
    ) -> Result<Vec<(String, i64, i64, i64)>> {
        let table_exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'gain_ledger'",
            [],
            |r| r.get(0),
        )?;
        if table_exists == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(items), 0), COALESCE(SUM(tokens), 0)
             FROM gain_ledger
             WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR created_at >= ?2)
             GROUP BY kind ORDER BY kind",
        )?;
        let rows = stmt.query_map(params![project, since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
