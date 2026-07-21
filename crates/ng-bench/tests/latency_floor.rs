//! Gate de regressão de latência do hot path do hook (plano 005).
//!
//! O invariante "<5ms por prompt" do `ng-hook` não era imposto por nada
//! executável: o bench criterion é informativo e o CI só roda fmt/clippy/test.
//! Este teste mede o caminho completo que o hook paga por prompt quando o
//! daemon está fora — `Store::open_bounded` (abrir + pragmas) mais
//! `build_injection` (busca + dedup + formatação) — sobre um corpus
//! determinístico de 10k eventos, e falha se a MEDIANA de 30 iterações
//! estourar o piso.
//!
//! O piso é 25ms: 5× o orçamento real de 5ms, folga deliberada para runners
//! de CI ruidosos. Timing em build debug não significa nada, então o teste é
//! `ignored` fora de release.

use std::time::Instant;

use ng_core::{lex, Event, Store};
use ng_hook::inject::build_injection;
use tempfile::tempdir;

/// Piso do gate em milissegundos (orçamento real do hook: 5ms).
const FLOOR_MS: f64 = 25.0;
/// Tamanho do corpus — grande o bastante para o custo de FTS aparecer.
const CORPUS_EVENTS: usize = 10_000;
/// Iterações medidas; a mediana absorve outliers de scheduling do runner.
const ITERATIONS: usize = 30;

/// Mesmo shape de seed do `inject_bench.rs` de ng-hook: templates
/// determinísticos, tags léxicas reais, 32 sessões intercaladas.
const TEMPLATES: &[&str] = &[
    "corrigir bug de autenticação no login: token JWT expira antes do refresh (evento {i})",
    "cache redis invalidado com TTL de 300s após deploy do serviço de sessão {i}",
    "docker build falhou na camada de dependências; layer cache não reaproveitado {i}",
    "migração sqlite: ALTER TABLE events ADD COLUMN tokens_est INTEGER NOT NULL {i}",
    "runtime tokio: task async trava aguardando await em canal mpsc fechado {i}",
];

/// Evento raro intercalado a cada [`RARE_EVERY`] eventos. Os templates acima
/// aparecem em ~20% do corpus cada, então o pruning de IDF de
/// `selective_fts_query` (corte em 5% de document frequency) descarta TODOS
/// os termos deles — a busca ficaria vazia e o gate mediria um caminho sem
/// FTS. Estes termos raros (~0.2% do corpus) sobrevivem ao pruning e mantêm
/// o caminho medido idêntico ao real: seleção de termos + MATCH + bm25 +
/// dedup + formatação.
const RARE_TEMPLATE: &str =
    "deadlock no scheduler zephyr: mutex de quorum preso durante failover do raft {i}";
const RARE_EVERY: usize = 500;

fn seed(path: &std::path::Path, n: usize) {
    let store = Store::open(path).expect("abrir store de seed");
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
        store.insert_event(&event).expect("inserir evento de seed");
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing só faz sentido em release")]
fn hook_hot_path_median_under_floor() {
    let dir = tempdir().expect("criar tempdir");
    let db = dir.path().join("ng.db");
    seed(&db, CORPUS_EVENTS);

    // Sessão ausente do corpus para que nada seja excluído como "atual".
    let session = "gate-session";
    let prompt = "deadlock de mutex no quorum do raft durante failover do scheduler";

    let mut samples_ms: Vec<f64> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let store = Store::open_bounded(&db).expect("open_bounded no hot path");
        let out = build_injection(&store, prompt, session);
        let elapsed = start.elapsed();
        assert!(
            out.is_some(),
            "gate mediria um caminho vazio: build_injection não retornou nada \
             para um prompt com overlap lexical direto com o corpus"
        );
        samples_ms.push(elapsed.as_secs_f64() * 1000.0);
    }

    samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("latências são finitas"));
    let median_ms = samples_ms[ITERATIONS / 2];

    // Visível com `--nocapture`: deixa o número no log do CI para comparação
    // histórica sem precisar rodar o bench criterion.
    eprintln!(
        "gate de latência: mediana {median_ms:.2}ms (piso {FLOOR_MS}ms, \
         orçamento real 5ms, corpus {CORPUS_EVENTS} eventos)"
    );

    assert!(
        median_ms < FLOOR_MS,
        "hot path mediano {median_ms:.2}ms estourou o piso de {FLOOR_MS}ms \
         (orçamento real: 5ms; corpus: {CORPUS_EVENTS} eventos, {ITERATIONS} iterações)"
    );
}
