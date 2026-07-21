// Each bench binary pulls in this whole module via `mod common;` but uses
// only the seeders it needs, so the others read as dead code per-binary.
#![allow(dead_code)]
//! Shared deterministic seeding for the ng-core criterion benches.
//!
//! No `rand`, no wall clock: every event's content and `created_at` are a
//! pure function of its index, so a bench run is byte-for-byte reproducible
//! across machines and CI. Content mixes pt-BR prose with code fragments to
//! mirror what harness transcripts actually store.

use ng_core::{lex, Embedder, Event, HashEmbedder, Store};
use std::path::Path;

/// Rotating pool of realistic pt-BR + code snippets. The index selects a
/// template and interpolates `i`, so the FTS index sees varied but
/// deterministic vocabulary instead of one repeated string.
const TEMPLATES: &[&str] = &[
    "corrigir bug de autenticação no login: token JWT expira antes do refresh (evento {i})",
    "cache redis invalidado com TTL de 300s após deploy do serviço de sessão {i}",
    "docker build falhou na camada de dependências; layer cache não reaproveitado {i}",
    "migração sqlite: ALTER TABLE events ADD COLUMN tokens_est INTEGER NOT NULL {i}",
    "runtime tokio: task async trava aguardando await em canal mpsc fechado {i}",
    "fn handle_request(req: Request) -> Result<Response, Error> {{ /* evento {i} */ }}",
    "revisão de código: extrair função grande em módulos menores, alta coesão {i}",
    "teste de integração cobrindo o endpoint POST /api/search retornou 500 no caso {i}",
    "configuração do WAL do sqlite: busy_timeout antes de journal_mode = WAL {i}",
    "embedding hash de trigramas normalizado por L2 para rerank híbrido {i}",
];

/// Build one deterministic event for corpus index `i`.
fn event_at(i: usize) -> Event {
    let content = TEMPLATES[i % TEMPLATES.len()].replace("{i}", &i.to_string());
    let tags = lex::extract_tags(&content);
    Event {
        session_id: format!("sess-{}", i % 32),
        project: "/tmp/not-goldfish-bench".to_string(),
        harness: "claude-code".to_string(),
        kind: "prompt".to_string(),
        content,
        tags,
        meta: None,
        // Fixed epoch base, one minute apart — deterministic, monotonic.
        created_at: 1_700_000_000 + (i as i64) * 60,
    }
}

/// Open a fresh store at `path` and insert `n` deterministic events.
/// Returns the store ready for FTS benches.
pub fn seed_store(path: &Path, n: usize) -> Store {
    let store = Store::open(path).unwrap();
    for i in 0..n {
        store.insert_event(&event_at(i)).unwrap();
    }
    store
}

/// Open + seed a store and populate `HashEmbedder` embeddings for every
/// event, so `search_hybrid` measures the real reranked hot path rather
/// than the degraded "no embedding" branch.
pub fn seed_store_with_embeddings(path: &Path, n: usize) -> Store {
    let store = seed_store(path, n);
    let embedder = HashEmbedder;
    let mut backlog = store.events_without_embedding(embedder.id(), n).unwrap();
    backlog.sort_by_key(|(id, _)| *id);
    let items: Vec<(i64, Vec<f32>)> = backlog
        .into_iter()
        .map(|(id, content)| (id, embedder.embed(&content)))
        .collect();
    store
        .upsert_embeddings_batch(embedder.id(), &items)
        .unwrap();
    store
}

/// Seed a store and fold every event into the wisdom graph, returning the
/// store plus the name of a high-degree entity usable as a `neighbors`
/// start node.
pub fn seed_store_with_graph(path: &Path, n: usize) -> Store {
    let store = seed_store(path, n);
    for i in 0..n {
        store.ingest_graph(&event_at(i)).unwrap();
    }
    store
}
