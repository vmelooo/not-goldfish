//! Operações de saver externo (plano 004): estado do gate de medição
//! (`saver_state`) e colunas derivadas `saved_*` de `events`.
//!
//! Invariante em todas as escritas daqui: `events.content` nunca é tocado.
//! O digest é uma projeção aditiva; o original permanece recuperável pelo
//! banco sempre, com ou sem `saved_ref`.

use rusqlite::{params, OptionalExtension};

use super::util::now_epoch;
use super::Store;
use crate::Result;

impl Store {
    /// Status atual de um saver (`measured`/`trusted`/`demoted`), ou `None`
    /// se nunca passou pelo bench. Tolerante a banco antigo read-only sem a
    /// tabela — isso é "nunca medido", não um erro.
    pub fn saver_status(&self, name: &str) -> Result<Option<String>> {
        if !self.saver_state_table_exists()? {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT status FROM saver_state WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Grava (upsert) o status de um saver no gate de medição.
    pub fn set_saver_status(&self, name: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO saver_state (name, status, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET status = excluded.status,
                                             updated_at = excluded.updated_at",
            params![name, status, now_epoch()],
        )?;
        Ok(())
    }

    /// Todos os estados registrados: `(name, status, updated_at)`.
    pub fn saver_states(&self) -> Result<Vec<(String, String, i64)>> {
        if !self.saver_state_table_exists()? {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT name, status, updated_at FROM saver_state ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn saver_state_table_exists(&self) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'saver_state'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Backlog do worker de saver: eventos `tool_output` grandes o bastante
    /// para valer um digest e ainda não tentados por nenhum saver
    /// (`saved_by IS NULL` cobre sucesso E falha anterior — falha não é
    /// re-tentada a cada poll). Ocultos ficam de fora, como na busca.
    /// Devolve `(id, project, content)` — o worker precisa do projeto para
    /// aplicar os toggles do `.ng/config.toml` daquele projeto.
    pub fn events_for_saver(
        &self,
        min_bytes: usize,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project, content FROM events
             WHERE kind = 'tool_output' AND saved_by IS NULL AND hidden_at IS NULL
               AND length(content) >= ?1
             ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![min_bytes as i64, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Grava o resultado de uma tentativa de saver num evento. Sucesso =
    /// digest (e ref opcional); falha = só `saved_by`, marcando "tentado,
    /// pass-through" sem re-tentativa. `content` fica intocado nos dois
    /// casos — é o invariante deste módulo.
    pub fn record_saver_result(
        &self,
        event_id: i64,
        digest: Option<&str>,
        saver_ref: Option<&str>,
        saver: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE events SET saved_digest = ?2, saved_ref = ?3, saved_by = ?4 WHERE id = ?1",
            params![event_id, digest, saver_ref, saver],
        )?;
        Ok(())
    }

    /// Colunas derivadas de saver de um evento:
    /// `(saved_digest, saved_ref, saved_by)`. É o que um builder de stub
    /// consultará quando o consumo (plano 004 etapa 6) for ligado — leitura
    /// pura de coluna pré-computada, nunca uma chamada viva de saver.
    pub fn saver_columns(
        &self,
        event_id: i64,
    ) -> Result<(Option<String>, Option<String>, Option<String>)> {
        Ok(self.conn.query_row(
            "SELECT saved_digest, saved_ref, saved_by FROM events WHERE id = ?1",
            params![event_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    /// Amostra de `tool_output` reais para o `ng saver bench` (mais
    /// recentes primeiro — o workload de hoje, não o de seis meses atrás).
    pub fn sample_tool_outputs(&self, min_bytes: usize, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM events
             WHERE kind = 'tool_output' AND hidden_at IS NULL AND length(content) >= ?1
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![min_bytes as i64, limit as i64], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Event, Store};

    fn open_temp() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("ng.db")).unwrap();
        (tmp, store)
    }

    fn tool_output(content: &str) -> Event {
        Event {
            session_id: "s1".into(),
            project: "/p".into(),
            harness: "claude-code".into(),
            kind: "tool_output".into(),
            content: content.into(),
            tags: String::new(),
            meta: None,
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn saver_status_roundtrips_and_defaults_to_none() {
        let (_tmp, store) = open_temp();
        assert_eq!(store.saver_status("headroom").unwrap(), None);
        store.set_saver_status("headroom", "measured").unwrap();
        assert_eq!(
            store.saver_status("headroom").unwrap().as_deref(),
            Some("measured")
        );
        store.set_saver_status("headroom", "trusted").unwrap();
        assert_eq!(
            store.saver_status("headroom").unwrap().as_deref(),
            Some("trusted")
        );
        let states = store.saver_states().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].0, "headroom");
        assert_eq!(states[0].1, "trusted");
    }

    #[test]
    fn events_for_saver_filters_kind_size_and_already_tried() {
        let (_tmp, store) = open_temp();
        let big = "x".repeat(100);
        store.insert_event(&tool_output(&big)).unwrap();
        store.insert_event(&tool_output("pequeno")).unwrap();
        let mut prompt = tool_output(&big);
        prompt.kind = "prompt".into();
        store.insert_event(&prompt).unwrap();

        let backlog = store.events_for_saver(50, 10).unwrap();
        assert_eq!(backlog.len(), 1, "só o tool_output grande entra");
        let (id, project, content) = &backlog[0];
        assert_eq!(content, &big);
        assert!(
            !project.is_empty(),
            "projeto acompanha o evento (toggles por projeto)"
        );

        // Falha registrada (digest NULL) tira o evento do backlog — e o
        // conteúdo original permanece byte-idêntico (pass-through).
        store
            .record_saver_result(*id, None, None, "meu-saver")
            .unwrap();
        assert!(store.events_for_saver(50, 10).unwrap().is_empty());
        let stored: String = {
            let mem = store.list_memories(None, false, 10).unwrap();
            mem.iter()
                .find(|m| m.id == *id)
                .map(|m| m.content.clone())
                .unwrap()
        };
        assert_eq!(stored, big, "content nunca é tocado por um saver");
    }

    #[test]
    fn record_saver_result_success_keeps_original_content() {
        let (_tmp, store) = open_temp();
        let big = "conteúdo original ".repeat(20);
        let id = store.insert_event(&tool_output(&big)).unwrap();
        store
            .record_saver_result(id, Some("digest curto"), Some("meu-saver:abc"), "meu-saver")
            .unwrap();
        let mem = store.list_memories(None, false, 10).unwrap();
        let row = mem.iter().find(|m| m.id == id).unwrap();
        assert_eq!(
            row.content, big,
            "digest é coluna aditiva, não substituição"
        );
    }

    #[test]
    fn sample_tool_outputs_returns_recent_large_outputs() {
        let (_tmp, store) = open_temp();
        store.insert_event(&tool_output(&"a".repeat(80))).unwrap();
        store.insert_event(&tool_output("tiny")).unwrap();
        let sample = store.sample_tool_outputs(50, 10).unwrap();
        assert_eq!(sample.len(), 1);
    }
}
