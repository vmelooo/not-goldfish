//! Background embedding worker.
//!
//! Runs on its own `Store` handle (SQLite WAL allows one writer thread and
//! this reader/writer to coexist) so it never contends with `writer_loop`'s
//! insert path — the daemon must stay responsive to hook traffic regardless
//! of how large the embedding backlog gets.

use std::path::PathBuf;
use std::time::Duration;

use ng_adapters::saver_cli::{build_enabled_savers, EnabledSaver};
use ng_adapters::savers::SaversConfig;
use ng_adapters::watcher::{scan_transcripts, ImportedEvent};
use ng_core::saver::{sanitize_digest, Saver};
use ng_core::{lex, Embedder, Event, HashEmbedder, Store};

const BATCH_SIZE: usize = 64;
const GRAPH_BATCH_SIZE: usize = 64;
/// Savers externos (plano 004): só tool_output com pelo menos este tamanho
/// vale um digest — abaixo disso o stub nativo já é praticamente do mesmo
/// tamanho do conteúdo.
const SAVER_MIN_CONTENT_BYTES: usize = 2048;
/// Poucos itens por poll: cada um é um processo externo com timeout
/// próprio; o worker é serial de propósito (no máximo 1 processo de saver
/// vivo por vez).
const SAVER_BATCH_SIZE: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Codex has no hook API, so its transcripts are only ever polled, not
/// pushed — scanning every `POLL_INTERVAL` would be wasteful disk churn
/// for a harness whose files change on the order of minutes, not seconds.
/// 30 polls * 2s = ~1 minute.
const WATCHER_SCAN_EVERY_N_POLLS: u32 = 30;

/// Runs forever: every `POLL_INTERVAL`, embed up to `BATCH_SIZE` events
/// that don't have a vector yet for the current embedder's model id, fold
/// up to `GRAPH_BATCH_SIZE` new events into the wisdom graph, and every
/// [`WATCHER_SCAN_EVERY_N_POLLS`] polls, import anything new from Codex's
/// own transcript files (Codex has no hook to push events through).
pub fn run(db_path: PathBuf) {
    // The writer thread opens its own connection at almost the same instant
    // (both spawned from `main` back to back); Store::open already retries
    // past that transient startup lock race, so a failure here means the
    // db is genuinely unusable, not a timing fluke.
    let store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("ngd: enrich: cannot open db: {err}");
            return;
        }
    };
    let embedder = HashEmbedder;
    // watcher-state.json lives next to the db rather than being derived
    // from `ng_core::paths` again: `db_path` is already this process's one
    // source of truth for "where does not-goldfish keep its state".
    let watcher_state_path = db_path
        .parent()
        .map(|dir| dir.join("watcher-state.json"))
        .unwrap_or_else(|| PathBuf::from("watcher-state.json"));
    // Savers externos: carregados uma vez no boot, do savers.toml GLOBAL ao
    // lado do banco (nunca de um arquivo de repositório — regra de origem
    // do plano 004 §5). Ausente/ inválido = nenhum saver, daemon segue.
    let (savers_config, savers) = load_savers(&db_path);
    let mut poll_count: u32 = 0;

    loop {
        match enrich_batch(&store, &embedder) {
            Ok(0) => {}
            Ok(n) => eprintln!("ngd: enrich: embedded {n} events (model {})", embedder.id()),
            Err(err) => eprintln!("ngd: enrich: batch failed: {err}"),
        }
        // Drena o backlog do grafo: depois de um wipe de versão, o histórico
        // inteiro está pendente — a 64/poll levaria horas. Sem backlog, uma
        // iteração única com n < GRAPH_BATCH_SIZE sai imediatamente.
        loop {
            match store.graph_ingest_pending(GRAPH_BATCH_SIZE) {
                Ok(n) => {
                    if n > 0 {
                        eprintln!("ngd: enrich: ingested {n} events into wisdom graph");
                    }
                    if n < GRAPH_BATCH_SIZE {
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("ngd: enrich: graph ingest failed: {err}");
                    break;
                }
            }
        }
        let imported = crate::assist::import_pending(&store, 8);
        if imported > 0 {
            eprintln!("ngd: assist: imported {imported} assistant turns");
        }
        if !savers.is_empty() {
            let n = saver_batch(&store, &savers_config, &savers);
            if n > 0 {
                eprintln!("ngd: enrich: saver digests computed for {n} events");
            }
        }

        poll_count = poll_count.wrapping_add(1);
        if poll_count.is_multiple_of(WATCHER_SCAN_EVERY_N_POLLS) {
            scan_codex(&store, &watcher_state_path);
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Carrega os savers externos habilitados do `savers.toml` global (mesmo
/// diretório do banco). Default OFF por construção: sem arquivo, sem seção
/// ou sem `enabled = true`, a lista volta vazia e nada roda. Ambos os
/// transportes (CLI e MCP, plano 004 §2a/§2b) entram atrás do mesmo trait
/// `Saver`. Devolve também a config parseada: `saver_batch` a clona por
/// projeto para aplicar os toggles do `.ng/config.toml` (só liga/desliga
/// e ajusta budget de savers já definidos globalmente — comandos nunca).
fn load_savers(db_path: &std::path::Path) -> (SaversConfig, Vec<EnabledSaver>) {
    let config_path = db_path
        .parent()
        .map(|dir| dir.join("savers.toml"))
        .unwrap_or_else(|| PathBuf::from("savers.toml"));
    let Ok(raw) = std::fs::read_to_string(&config_path) else {
        return (SaversConfig::default(), Vec::new());
    };
    let config = match SaversConfig::from_global_toml(&raw) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("ngd: enrich: savers.toml inválido, nenhum saver ativo: {err}");
            return (SaversConfig::default(), Vec::new());
        }
    };
    let (built, skipped) = build_enabled_savers(&config);
    for name in skipped {
        eprintln!("ngd: enrich: saver {name} inválido, pulado");
    }
    (config, built)
}

/// Um passo de saver do worker: para cada saver com status `trusted` no
/// gate de medição (`ng saver bench` promove; nada roda sem número),
/// computa digests para tool_output grandes ainda não tentados e grava nas
/// colunas derivadas `saved_*`. `events.content` NUNCA é tocado: em falha/
/// timeout o evento fica byte-idêntico (pass-through) e só `saved_by`
/// marca a tentativa.
///
/// Seam de consumo (plano 004 etapa 6, não implementado nesta fase): os
/// builders de stub (`ng_sessions::hygiene::stub_for`, `ngd::ui`) ainda
/// não preferem `saved_digest` ao stub nativo `[ng-evicted: …]`; quando
/// passarem a preferir, é SÓ leitura de coluna pré-computada — nenhuma
/// chamada viva de saver entra em hook/PreCompact.
fn saver_batch(
    store: &Store,
    global_config: &SaversConfig,
    savers: &[(Box<dyn Saver>, i64)],
) -> usize {
    let mut computed = 0;
    // Cache de config efetiva por projeto: o `.ng/config.toml` do projeto
    // (arquivo de repositório, potencialmente não-confiável) só liga/
    // desliga e re-orça savers globais — `apply_project_toggles` rejeita
    // qualquer outra chave, e um arquivo inválido/erro é ignorado
    // (fail-closed: vale o global).
    let mut project_configs: std::collections::HashMap<String, Option<SaversConfig>> =
        std::collections::HashMap::new();
    for (saver, budget) in savers {
        match store.saver_status(saver.name()) {
            Ok(Some(status)) if status == "trusted" => {}
            Ok(_) => continue, // nunca medido / measured / demoted: não roda
            Err(err) => {
                eprintln!("ngd: enrich: saver_state read failed: {err}");
                continue;
            }
        }
        let backlog = match store.events_for_saver(SAVER_MIN_CONTENT_BYTES, SAVER_BATCH_SIZE) {
            Ok(backlog) => backlog,
            Err(err) => {
                eprintln!("ngd: enrich: saver backlog query failed: {err}");
                continue;
            }
        };
        for (event_id, project, content) in backlog {
            let effective = project_configs
                .entry(project.clone())
                .or_insert_with(|| {
                    let raw = std::fs::read_to_string(
                        std::path::Path::new(&project)
                            .join(".ng")
                            .join("config.toml"),
                    )
                    .ok()?;
                    let mut cfg = global_config.clone();
                    match cfg.apply_project_toggles(&raw) {
                        Ok(()) => Some(cfg),
                        Err(err) => {
                            eprintln!("ngd: enrich: .ng/config.toml de {project} ignorado ({err})");
                            None
                        }
                    }
                })
                .as_ref();
            let (enabled, budget) = effective
                .and_then(|cfg| cfg.savers.iter().find(|s| s.name == saver.name()))
                .map(|s| (s.enabled, s.budget_tokens))
                .unwrap_or((true, *budget));
            if !enabled {
                continue;
            }
            let result = match saver.compress(&content, budget) {
                Ok(c) => {
                    // Saída é dado, não instrução: sanitizada e re-capada
                    // antes de virar coluna que um dia entra num stub.
                    let digest = sanitize_digest(&c.text, (budget as usize).saturating_mul(4));
                    let saver_ref = c.reversible_ref.map(|r| r.to_string());
                    computed += 1;
                    store.record_saver_result(
                        event_id,
                        Some(&digest),
                        saver_ref.as_deref(),
                        saver.name(),
                    )
                }
                Err(err) => {
                    eprintln!(
                        "ngd: enrich: saver {} failed on event {event_id} (pass-through): {err}",
                        saver.name()
                    );
                    store.record_saver_result(event_id, None, None, saver.name())
                }
            };
            if let Err(err) = result {
                eprintln!("ngd: enrich: saver result write failed: {err}");
            }
        }
    }
    computed
}

fn enrich_batch(store: &Store, embedder: &dyn Embedder) -> ng_core::Result<usize> {
    let backlog = store.events_without_embedding(embedder.id(), BATCH_SIZE)?;
    // Embed em memória primeiro, depois um único upsert transacionado para o
    // lote inteiro — um fsync por batch em vez de um por evento.
    let items: Vec<(i64, Vec<f32>)> = backlog
        .into_iter()
        .map(|(event_id, content)| (event_id, embedder.embed(&content)))
        .collect();
    store.upsert_embeddings_batch(embedder.id(), &items)?;
    Ok(items.len())
}

/// Scans `~/.codex/sessions` (only if it exists — most machines running
/// `ngd` never have Codex installed) for anything new since the last scan
/// and inserts it through the same connection `enrich_batch` uses. Dedup
/// is inherent: `scan_transcripts`'s own incremental state (persisted at
/// `state_path`) guarantees each transcript item is ever returned once.
fn scan_codex(store: &Store, state_path: &std::path::Path) {
    let Some(home) = dirs::home_dir() else { return };
    let root = home.join(".codex").join("sessions");
    if !root.exists() {
        return;
    }
    match scan_transcripts(&[root], state_path) {
        Ok(imported) if imported.is_empty() => {}
        Ok(imported) => {
            let mut inserted = 0;
            for item in &imported {
                let event = imported_event_to_event(item);
                match store.insert_event(&event) {
                    Ok(_) => inserted += 1,
                    Err(err) => eprintln!("ngd: enrich: codex import insert failed: {err}"),
                }
            }
            eprintln!("ngd: enrich: imported {inserted} events from codex watcher");
        }
        Err(err) => eprintln!("ngd: enrich: codex watcher scan failed: {err}"),
    }
}

/// Maps a watcher-imported transcript item onto `ng_core::Event`.
///
/// Kind is derived from `ImportedEvent::role` (mirroring
/// `SessionItem::role` in `ng-sessions`): `user -> prompt`, `assistant ->
/// assistant`, `tool -> tool_output`, `system -> system`, anything else
/// `-> other`. The `system`/`other` kinds keep those events out of both
/// the wisdom graph (whose extractor dispatches by kind) and the
/// injection search (whose kind whitelist doesn't include them). Raw
/// mechanical content (JSON payloads, code dumps) imports with empty
/// tags so it can't pollute lexical tag search.
fn imported_event_to_event(imported: &ImportedEvent) -> Event {
    let tags = if lex::is_mostly_code(&imported.content) {
        String::new()
    } else {
        lex::extract_tags(&imported.content)
    };
    Event {
        session_id: imported.session_id.clone(),
        // ImportedEvent carries no cwd/project at this layer (transcript
        // files aren't tagged with the project they belong to); global
        // scope ("") is the honest default rather than guessing one.
        project: String::new(),
        harness: imported.harness.clone(),
        kind: map_imported_role(&imported.role).to_string(),
        content: imported.content.clone(),
        tags,
        meta: None,
        created_at: imported.created_at,
    }
    .cap_content()
}

fn map_imported_role(role: &str) -> &'static str {
    match role {
        "user" => "prompt",
        "assistant" => "assistant",
        "tool" => "tool_output",
        // System prompts and unknown roles get their own kinds so the graph
        // and injection filters can exclude them by name instead of them
        // masquerading as tool output (which the typed extractor DOES scan).
        "system" => "system",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported(role: &str, kind: &str, content: &str) -> ImportedEvent {
        ImportedEvent {
            session_id: "sess-1".to_string(),
            harness: "codex".to_string(),
            role: role.to_string(),
            kind: kind.to_string(),
            content: content.to_string(),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn user_role_maps_to_prompt() {
        let event =
            imported_event_to_event(&imported("user", "text", "vamos usar rusqlite sempre"));
        assert_eq!(event.kind, "prompt");
    }

    #[test]
    fn assistant_role_maps_to_assistant() {
        let event =
            imported_event_to_event(&imported("assistant", "text", "vamos usar rusqlite sempre"));
        assert_eq!(event.kind, "assistant");
    }

    #[test]
    fn tool_role_maps_to_tool_output() {
        let event = imported_event_to_event(&imported("tool", "function_call", "[ls] -la"));
        assert_eq!(event.kind, "tool_output");
    }

    #[test]
    fn system_role_maps_to_system_kind() {
        let event = imported_event_to_event(&imported("system", "text", "session started"));
        assert_eq!(event.kind, "system");
    }

    #[test]
    fn unknown_role_maps_to_other_kind() {
        let event = imported_event_to_event(&imported("weird_future_role", "text", "?"));
        assert_eq!(event.kind, "other");
    }

    #[test]
    fn raw_json_content_gets_no_tags() {
        let raw = r#"{"type":"function_call_output","call_id":"abc","output":{"ok":true}}"#;
        let event = imported_event_to_event(&imported("tool", "function_call_output", raw));
        assert_eq!(event.tags, "");
    }

    #[test]
    fn preserves_harness_session_and_timestamp() {
        let event = imported_event_to_event(&imported("user", "text", "oi"));
        assert_eq!(event.harness, "codex");
        assert_eq!(event.session_id, "sess-1");
        assert_eq!(event.created_at, 1_700_000_000);
    }

    struct FakeSaver {
        fail: bool,
    }

    impl Saver for FakeSaver {
        fn name(&self) -> &str {
            "fake"
        }
        fn compress(&self, input: &str, budget: i64) -> ng_core::Result<ng_core::Compressed> {
            if self.fail {
                return Err(ng_core::Error::Other("saver quebrado".into()));
            }
            ng_core::Compressed::from_input(input, "digest de teste".into(), budget, None)
        }
        fn retrieve(&self, _r: &ng_core::SaverRef) -> ng_core::Result<String> {
            Err(ng_core::Error::Other("sem retrieve".into()))
        }
    }

    fn store_with_big_tool_output_in(project: &str) -> (tempfile::TempDir, Store, i64, String) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("ng.db")).unwrap();
        let content = "saida grande de ferramenta ".repeat(200); // > 2 KiB
        let event = Event {
            session_id: "s1".into(),
            project: project.into(),
            harness: "claude-code".into(),
            kind: "tool_output".into(),
            content: content.clone(),
            tags: String::new(),
            meta: None,
            created_at: 1_700_000_000,
        };
        let id = store.insert_event(&event).unwrap();
        (tmp, store, id, content)
    }

    fn store_with_big_tool_output() -> (tempfile::TempDir, Store, i64, String) {
        store_with_big_tool_output_in("/p")
    }

    #[test]
    fn saver_batch_only_runs_trusted_savers() {
        let (_tmp, store, id, _content) = store_with_big_tool_output();
        let savers: Vec<(Box<dyn Saver>, i64)> = vec![(Box::new(FakeSaver { fail: false }), 64)];

        // Sem status (nunca medido) e como "measured": nada roda.
        assert_eq!(saver_batch(&store, &SaversConfig::default(), &savers), 0);
        store.set_saver_status("fake", "measured").unwrap();
        assert_eq!(saver_batch(&store, &SaversConfig::default(), &savers), 0);
        assert_eq!(store.saver_columns(id).unwrap(), (None, None, None));

        // Só "trusted" (gate do bench) libera o digest.
        store.set_saver_status("fake", "trusted").unwrap();
        assert_eq!(saver_batch(&store, &SaversConfig::default(), &savers), 1);
        let (digest, _ref, by) = store.saver_columns(id).unwrap();
        assert_eq!(digest.as_deref(), Some("digest de teste"));
        assert_eq!(by.as_deref(), Some("fake"));
    }

    #[test]
    fn saver_batch_failure_is_byte_identical_passthrough() {
        let (_tmp, store, id, content) = store_with_big_tool_output();
        store.set_saver_status("fake", "trusted").unwrap();
        let savers: Vec<(Box<dyn Saver>, i64)> = vec![(Box::new(FakeSaver { fail: true }), 64)];
        assert_eq!(saver_batch(&store, &SaversConfig::default(), &savers), 0);

        // Falha marcada (não re-tentada), digest ausente, e o conteúdo
        // original permanece byte-idêntico — captura nunca quebra.
        let (digest, saver_ref, by) = store.saver_columns(id).unwrap();
        assert_eq!(digest, None);
        assert_eq!(saver_ref, None);
        assert_eq!(by.as_deref(), Some("fake"));
        let mem = store.list_memories(None, false, 10).unwrap();
        assert_eq!(mem.iter().find(|m| m.id == id).unwrap().content, content);
        assert!(store.events_for_saver(2048, 8).unwrap().is_empty());
    }

    fn fake_global_config() -> SaversConfig {
        SaversConfig {
            savers: vec![ng_adapters::savers::SaverSpec {
                name: "fake".into(),
                enabled: true,
                transport: ng_adapters::savers::Transport::Cli,
                command: vec!["fake".into()],
                retrieve_command: vec![],
                timeout_ms: 2000,
                max_input_bytes: 1_048_576,
                max_output_bytes: 65_536,
                budget_tokens: 64,
                apply_to: vec!["tool_output".into()],
                tools: None,
            }],
        }
    }

    #[test]
    fn saver_batch_respects_project_toggle_disable() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project = tmpdir.path().to_string_lossy().to_string();
        let (_tmp, store, id, _content) = store_with_big_tool_output_in(&project);
        // O evento fica no projeto do tmpdir, que declara `enabled = false`.
        std::fs::create_dir_all(tmpdir.path().join(".ng")).unwrap();
        std::fs::write(
            tmpdir.path().join(".ng/config.toml"),
            "[savers.fake]\nenabled = false\n",
        )
        .unwrap();

        store.set_saver_status("fake", "trusted").unwrap();
        let savers: Vec<(Box<dyn Saver>, i64)> = vec![(Box::new(FakeSaver { fail: false }), 64)];
        assert_eq!(saver_batch(&store, &fake_global_config(), &savers), 0);
        assert_eq!(store.saver_columns(id).unwrap(), (None, None, None));
    }

    #[test]
    fn saver_batch_applies_project_budget_override() {
        struct BudgetRecorder(std::sync::Arc<std::sync::Mutex<Vec<i64>>>);
        impl Saver for BudgetRecorder {
            fn name(&self) -> &str {
                "fake"
            }
            fn compress(&self, input: &str, budget: i64) -> ng_core::Result<ng_core::Compressed> {
                self.0.lock().unwrap().push(budget);
                ng_core::Compressed::from_input(input, "digest de teste".into(), budget, None)
            }
            fn retrieve(&self, _r: &ng_core::SaverRef) -> ng_core::Result<String> {
                Err(ng_core::Error::Other("sem retrieve".into()))
            }
        }

        let tmpdir = tempfile::tempdir().unwrap();
        let project = tmpdir.path().to_string_lossy().to_string();
        let (_tmp, store, id, _content) = store_with_big_tool_output_in(&project);
        std::fs::create_dir_all(tmpdir.path().join(".ng")).unwrap();
        std::fs::write(
            tmpdir.path().join(".ng/config.toml"),
            "[savers.fake]\nbudget_tokens = 32\n",
        )
        .unwrap();

        store.set_saver_status("fake", "trusted").unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let savers: Vec<(Box<dyn Saver>, i64)> = vec![(Box::new(BudgetRecorder(seen.clone())), 64)];
        assert_eq!(saver_batch(&store, &fake_global_config(), &savers), 1);
        // 32 = override do `.ng/config.toml` do projeto, não 64 do global.
        assert_eq!(*seen.lock().unwrap(), vec![32]);
        let (digest, _r, by) = store.saver_columns(id).unwrap();
        assert_eq!(digest.as_deref(), Some("digest de teste"));
        assert_eq!(by.as_deref(), Some("fake"));
    }

    #[test]
    fn load_savers_returns_empty_when_no_config_exists() {
        let tmp = tempfile::tempdir().unwrap();
        // Default OFF por construção: sem savers.toml, nenhum saver ativo.
        let (config, built) = load_savers(&tmp.path().join("ng.db"));
        assert!(built.is_empty());
        assert!(config.savers.is_empty());
    }

    #[test]
    fn extracts_tags_from_content() {
        let event = imported_event_to_event(&imported(
            "user",
            "text",
            "fix src/auth/login.rs bug please",
        ));
        assert!(event.tags.contains("src/auth/login.rs"));
    }
}
