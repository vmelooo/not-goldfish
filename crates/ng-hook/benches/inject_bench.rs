//! Latency benchmark for `build_injection` — the read half of the
//! UserPromptSubmit hot path (the hook's overall budget is <5ms). Seeded
//! with a deterministic 1k-event store so the number documents the search +
//! dedup + formatting cost the hook pays on every prompt.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ng_core::{lex, Event, Store};
use ng_hook::inject::build_injection;
use tempfile::tempdir;

const TEMPLATES: &[&str] = &[
    "corrigir bug de autenticação no login: token JWT expira antes do refresh (evento {i})",
    "cache redis invalidado com TTL de 300s após deploy do serviço de sessão {i}",
    "docker build falhou na camada de dependências; layer cache não reaproveitado {i}",
    "migração sqlite: ALTER TABLE events ADD COLUMN tokens_est INTEGER NOT NULL {i}",
    "runtime tokio: task async trava aguardando await em canal mpsc fechado {i}",
];

/// Evento raro (1 a cada 500): os templates acima dominam o corpus, então o
/// pruning de IDF de `selective_fts_query` (corte em 5% de df) descarta os
/// termos deles em corpora grandes e a busca vira o caminho vazio. Termos
/// raros sobrevivem ao pruning e mantêm o caso 10k medindo o caminho FTS
/// completo (o mesmo shape do gate em ng-bench/tests/latency_floor.rs).
const RARE_TEMPLATE: &str =
    "deadlock no scheduler zephyr: mutex de quorum preso durante failover do raft {i}";
const RARE_EVERY: usize = 500;

fn seed(path: &std::path::Path, n: usize) -> Store {
    let store = Store::open(path).unwrap();
    for i in 0..n {
        let template = if i % RARE_EVERY == 0 {
            RARE_TEMPLATE
        } else {
            TEMPLATES[i % TEMPLATES.len()]
        };
        let content = template.replace("{i}", &i.to_string());
        let event = Event {
            session_id: format!("sess-{}", i % 32),
            project: "/tmp/not-goldfish-bench".to_string(),
            harness: "claude-code".to_string(),
            kind: "prompt".to_string(),
            content: content.clone(),
            tags: lex::extract_tags(&content),
            meta: None,
            created_at: 1_700_000_000 + (i as i64) * 60,
        };
        store.insert_event(&event).unwrap();
    }
    store
}

fn bench_inject(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let store = seed(&dir.path().join("ng.db"), 1_000);
    // A session id absent from the corpus so nothing is excluded as "current".
    let session = "bench-session";
    let prompt = "bug de autenticação no login com token JWT expirado";

    c.bench_function("build_injection_1k", |b| {
        b.iter(|| {
            let out = build_injection(&store, black_box(prompt), black_box(session));
            black_box(out);
        });
    });

    // Caso 10k: corpus em que o pruning de IDF descarta os termos comuns —
    // o prompt usa os termos raros para que o bench meça o caminho FTS real
    // (MATCH + bm25 + dedup + formatação) em escala, não o retorno vazio.
    let dir_10k = tempdir().unwrap();
    let store_10k = seed(&dir_10k.path().join("ng.db"), 10_000);
    let rare_prompt = "deadlock de mutex no quorum do raft durante failover do scheduler";

    c.bench_function("build_injection_10k", |b| {
        b.iter(|| {
            let out = build_injection(&store_10k, black_box(rare_prompt), black_box(session));
            black_box(out);
        });
    });
}

criterion_group!(benches, bench_inject);
criterion_main!(benches);
