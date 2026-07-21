//! Daemon-side assistant capture: on session_end events carrying a
//! transcript_path (sent by the Stop/SessionEnd hooks), parse the
//! transcript in the background and capture new assistant turns as
//! `kind=assistant` events. This is the other half of the dialogue the
//! hooks can't push — Claude Code has no assistant-message hook. Never
//! runs in the hook hot path.

use std::path::Path;
use std::time::SystemTime;

use ng_core::{lex, Event, Store};
use ng_sessions::{SessionInfo, SessionItem};

/// Import assistant turns for up to `limit` pending session_end events.
/// Returns how many assistant events were inserted. Every per-session
/// failure is logged and skipped — a vanished transcript file must not
/// wedge the cursor forever, so the cursor advances past failures too, and
/// past rows the scan filtered out (meta without a parseable
/// transcript_path), via `max_scanned_id`.
pub fn import_pending(store: &Store, limit: usize) -> usize {
    let scan = match store.pending_transcript_imports(limit) {
        Ok(scan) => scan,
        Err(err) => {
            eprintln!("ngd: assist: pending query failed: {err}");
            return 0;
        }
    };
    let mut inserted = 0;
    for item in &scan.imports {
        match import_session(store, item) {
            Ok(n) => inserted += n,
            Err(err) => eprintln!(
                "ngd: assist: import of {} failed (skipped): {err}",
                item.transcript_path
            ),
        }
    }
    // max_scanned_id cobre também as linhas puladas pelo filtro de meta,
    // então é sempre >= qualquer event_id retornado — avança de uma vez.
    if scan.max_scanned_id > 0 {
        if let Err(err) = store.assist_cursor_set(scan.max_scanned_id) {
            eprintln!("ngd: assist: cursor advance failed: {err}");
        }
    }
    inserted
}

fn import_session(store: &Store, pending: &ng_core::PendingImport) -> anyhow::Result<usize> {
    let path = Path::new(&pending.transcript_path);
    let info = SessionInfo {
        id: pending.session_id.clone(),
        harness: pending.harness.clone(),
        path: path.to_path_buf(),
        project: Some(pending.project.clone()),
        modified_at: std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH),
        items_hint: None,
    };
    let transcript = ng_sessions::load_transcript(&info)?;
    // kind == "text" é o valor que o parser claude emite para turnos só de
    // texto; o Claude Code grava um content block por linha, então texto e
    // tool_use nunca dividem um item (blocos thinking viram itens próprios
    // e ficam de fora por construção).
    let assistant_items: Vec<&SessionItem> = transcript
        .items
        .iter()
        .filter(|item| {
            item.role == "assistant" && item.kind == "text" && !item.text_full.is_empty()
        })
        .collect();

    let already = store.transcript_imported_count(&pending.session_id)?;
    if assistant_items.len() <= already {
        return Ok(0);
    }
    let mut inserted = 0;
    for item in &assistant_items[already..] {
        let content = item.text_full.clone();
        let tags = if lex::is_mostly_code(&content) {
            String::new()
        } else {
            lex::extract_tags(&content)
        };
        let event = Event {
            session_id: pending.session_id.clone(),
            project: pending.project.clone(),
            harness: pending.harness.clone(),
            kind: "assistant".to_string(),
            content,
            tags,
            meta: None,
            created_at: item.timestamp.unwrap_or_else(now_epoch),
        }
        .cap_content();
        store.insert_event(&event)?;
        inserted += 1;
    }
    store.set_transcript_imported_count(&pending.session_id, assistant_items.len())?;
    Ok(inserted)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn write_claude_transcript(
        dir: &Path,
        session: &str,
        assistant_texts: &[&str],
    ) -> std::path::PathBuf {
        // Espelha o shape das linhas dos testes de
        // crates/ng-sessions/src/claude.rs — o parser real é a fonte da
        // verdade do formato: `type` no topo, `timestamp` RFC3339,
        // `message.content` como array de blocos para assistant.
        let path = dir.join(format!("{session}.jsonl"));
        let mut lines = vec![serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "oi"},
            "timestamp": "2026-07-21T12:00:00Z",
        })
        .to_string()];
        for text in assistant_texts {
            lines.push(
                serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
                    "timestamp": "2026-07-21T12:00:01Z",
                })
                .to_string(),
            );
        }
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn session_end_event(session: &str, transcript: &Path) -> ng_core::Event {
        ng_core::Event {
            session_id: session.into(),
            project: "/tmp/proj".into(),
            harness: "claude-code".into(),
            kind: "session_end".into(),
            content: String::new(),
            tags: String::new(),
            meta: Some(
                serde_json::json!({"transcript_path": transcript.to_string_lossy()}).to_string(),
            ),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn imports_new_assistant_turns_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("ng.db")).unwrap();
        let transcript =
            write_claude_transcript(tmp.path(), "s1", &["resposta um", "resposta dois"]);
        store
            .insert_event(&session_end_event("s1", &transcript))
            .unwrap();

        assert_eq!(import_pending(&store, 10), 2);
        // Segundo Stop sem turnos novos: nada re-importado.
        store
            .insert_event(&session_end_event("s1", &transcript))
            .unwrap();
        assert_eq!(import_pending(&store, 10), 0);
    }

    #[test]
    fn missing_transcript_advances_cursor_without_wedging() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("ng.db")).unwrap();
        let ghost = tmp.path().join("nao-existe.jsonl");
        store
            .insert_event(&session_end_event("s2", &ghost))
            .unwrap();
        assert_eq!(import_pending(&store, 10), 0);
        assert!(
            store
                .pending_transcript_imports(10)
                .unwrap()
                .imports
                .is_empty(),
            "cursor não avançou"
        );
    }

    #[test]
    fn meta_without_transcript_path_advances_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("ng.db")).unwrap();
        // Meta não-nulo mas sem transcript_path: a linha ocupa a janela do
        // LIMIT e é filtrada em Rust; o cursor precisa avançar sobre ela,
        // senão uma janela inteira dessas linhas trava a importação para
        // sempre.
        let mut end = session_end_event("s3", Path::new("/tmp/ignorado.jsonl"));
        end.meta = Some(r#"{"foo":1}"#.into());
        let id = store.insert_event(&end).unwrap();

        assert_eq!(import_pending(&store, 10), 0);
        assert_eq!(
            store.assist_cursor_get().unwrap(),
            id,
            "cursor deve avançar sobre linhas varridas mesmo sem path parseável"
        );
    }
}
