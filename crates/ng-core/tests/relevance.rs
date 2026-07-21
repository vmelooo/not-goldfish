//! Search-quality regression gate for ng-core ranking.
//!
//! This is NOT a latency benchmark — it runs in CI as a normal `#[test]`.
//! It builds a fixed, deterministic synthetic corpus (five well-separated
//! topics, each with a handful of known-relevant documents drowned in
//! distractors) and scores both retrieval paths against hand-labelled
//! ground truth.
//!
//! Metrics (averaged over all queries):
//! - **precision@3** — fraction of the top-3 results that are relevant.
//!   Measures how clean the very top of the ranking is (what injection
//!   actually shows the model).
//! - **recall@10** — fraction of a query's relevant documents that appear
//!   in the top-10. Measures whether relevant memory is retrievable at all.
//! - **MRR** — mean reciprocal rank of the first relevant hit. Measures how
//!   early the first good answer lands.
//!
//! The `assert!`s at the bottom are floors set slightly below observed
//! values, so the test fails if a change regresses ranking quality — it is
//! a gate, not a smoke test. Run with:
//!   `cargo test -p ng-core --test relevance -- --nocapture`

use std::collections::HashSet;

use ng_core::{lex, Embedder, Event, HashEmbedder, Store};
use tempfile::TempDir;

/// Insert one event with lexically-extracted tags, returning its id.
fn add(store: &Store, idx: usize, content: &str) -> i64 {
    let event = Event {
        session_id: format!("sess-{}", idx % 8),
        project: "/tmp/relevance".to_string(),
        harness: "claude-code".to_string(),
        kind: "prompt".to_string(),
        content: content.to_string(),
        tags: lex::extract_tags(content),
        meta: None,
        created_at: 1_700_000_000 + (idx as i64) * 60,
    };
    store.insert_event(&event).unwrap()
}

/// A labelled query: the text and the set of event ids that are actually
/// relevant to it (ground truth).
struct Labelled {
    name: &'static str,
    query: &'static str,
    relevant: HashSet<i64>,
}

/// One topic: a query, the documents genuinely relevant to it, and "hard
/// negatives" — distractors that share one or two query terms but are about
/// something else. The hard negatives are what make this a real ranking
/// test: they compete in the FTS candidate set, so a healthy ranker has to
/// keep the truly-relevant docs above them. The last relevant doc of each
/// topic is deliberately weak (only one or two query terms) to leave
/// ranking headroom instead of trivially perfect scores.
struct Topic {
    name: &'static str,
    query: &'static str,
    relevant: &'static [&'static str],
    hard_negatives: &'static [&'static str],
}

const TOPICS: &[Topic] = &[
    Topic {
        name: "auth",
        query: "autenticação login token JWT expira 401",
        relevant: &[
            "bug de autenticação: token JWT expira antes do refresh e o login retorna 401",
            "sessão de login inválida após o refresh do token de autenticação",
            "corrigir fluxo de autenticação renovando o token JWT no endpoint de login",
            "o servidor devolve 401 quando o token está expirado",
        ],
        hard_negatives: &[
            "gerar token de convite para novos usuários no painel administrativo",
            "tela de login social com Google e GitHub no aplicativo mobile",
            "o endpoint de health check devolve 401 sem corpo de resposta",
        ],
    },
    Topic {
        name: "redis-cache",
        query: "cache redis invalidação TTL sessão",
        relevant: &[
            "cache redis invalidado incorretamente, o TTL de 300s expira cedo demais",
            "invalidação de cache redis após deploy deixou chaves de sessão órfãs",
            "configurar o TTL do cache redis para 600s e evitar cache stampede",
            "a sessão do usuário some antes da hora",
        ],
        hard_negatives: &[
            "limpar o cache do navegador do usuário no frontend após deploy",
            "configurar o TTL do registro DNS do domínio de produção",
            "armazenar a sessão do usuário num cookie assinado",
        ],
    },
    Topic {
        name: "docker-build",
        query: "docker build imagem layer cache dependências",
        relevant: &[
            "docker build lento porque o layer cache das dependências não é reaproveitado",
            "otimizar o Dockerfile para melhorar o cache de camadas no docker build",
            "docker build falha ao copiar as dependências para a imagem final",
            "a imagem final ficou grande demais",
        ],
        hard_negatives: &[
            "corrigir o build do frontend com vite e esbuild em produção",
            "otimizar a imagem de capa do artigo do blog para SEO",
            "atualizar as dependências do projeto para a versão mais recente",
        ],
    },
    Topic {
        name: "sqlite-migration",
        query: "migração sqlite ALTER TABLE schema coluna",
        relevant: &[
            "migração sqlite: ALTER TABLE events ADD COLUMN tokens_est INTEGER",
            "schema sqlite desatualizado, rodar a migração antes do deploy",
            "migração falhou porque a coluna já existe no schema sqlite",
            "adicionar uma coluna nova na tabela de eventos",
        ],
        hard_negatives: &[
            "migração da equipe para o novo repositório git no GitLab",
            "ajustar a largura da coluna da tabela no CSS do relatório",
            "documentar o schema JSON do payload do webhook de entrada",
        ],
    },
    Topic {
        name: "rust-async",
        query: "rust async tokio await runtime task",
        relevant: &[
            "runtime tokio trava: a task async fica aguardando await num canal fechado",
            "deadlock no runtime async do tokio ao dar await segurando um mutex",
            "spawn de task tokio não conclui, o runtime async ficou sem worker threads",
            "a função precisa virar assíncrona",
        ],
        hard_negatives: &[
            "criar uma task no board de planejamento da sprint",
            "reduzir o tempo de runtime do pipeline de CI no GitHub",
            "escrever a documentação da API async pública do crate",
        ],
    },
];

/// Distractor templates: subjects with no vocabulary overlap with any query
/// above, so a healthy ranker never surfaces them for a topical query.
const NOISE: &[&str] = &[
    "atualizar o README com as instruções de instalação do binário",
    "revisar o PR de refatoração do módulo de interface web",
    "ajustar o espaçamento do componente de navbar no frontend",
    "escrever teste unitário para o parser de transcript do harness",
    "configurar o workflow de CI no GitHub Actions para rodar clippy",
    "documentar as variáveis de ambiente suportadas pelo daemon",
    "renomear a função de exportação de markdown do grafo de sabedoria",
    "medir o consumo de memória do worker de enriquecimento em background",
    "adicionar comando doctor para diagnosticar a instalação dos hooks",
    "melhorar a mensagem de erro quando o socket unix não está disponível",
    "trocar o ícone da bandeja e o tema escuro da interface local",
    "revisar a política de retenção de backups em disco dos transcripts",
];

fn build_corpus() -> (TempDir, Store, Vec<Labelled>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("ng.db")).unwrap();
    let mut idx = 0usize;
    let mut labelled = Vec::new();

    // Interleave relevant docs, hard negatives and generic noise so ids are
    // not clustered by topic — a ranker that just returned a contiguous id
    // block would be caught.
    for topic in TOPICS {
        let mut relevant = HashSet::new();
        for doc in topic.relevant {
            let id = add(&store, idx, doc);
            relevant.insert(id);
            idx += 1;
            // A couple of generic distractors between each relevant doc.
            for _ in 0..3 {
                add(&store, idx, NOISE[idx % NOISE.len()]);
                idx += 1;
            }
        }
        // Hard negatives: share query vocabulary but are NOT relevant. Their
        // ids never enter `relevant`, so surfacing them costs precision.
        for neg in topic.hard_negatives {
            add(&store, idx, neg);
            idx += 1;
        }
        labelled.push(Labelled {
            name: topic.name,
            query: topic.query,
            relevant,
        });
    }
    (dir, store, labelled)
}

/// (precision@3, recall@10, reciprocal_rank) for one ranked id list.
fn score(ranked: &[i64], relevant: &HashSet<i64>) -> (f64, f64, f64) {
    let top3 = ranked
        .iter()
        .take(3)
        .filter(|id| relevant.contains(id))
        .count();
    let precision_at_3 = top3 as f64 / 3.0;

    let top10_hits = ranked
        .iter()
        .take(10)
        .filter(|id| relevant.contains(id))
        .count();
    let recall_at_10 = top10_hits as f64 / relevant.len() as f64;

    let reciprocal_rank = ranked
        .iter()
        .position(|id| relevant.contains(id))
        .map(|pos| 1.0 / (pos as f64 + 1.0))
        .unwrap_or(0.0);

    (precision_at_3, recall_at_10, reciprocal_rank)
}

/// Averaged metrics over all queries for a given retrieval closure.
fn evaluate<F>(labelled: &[Labelled], mut retrieve: F) -> (f64, f64, f64)
where
    F: FnMut(&str) -> Vec<i64>,
{
    let (mut p3, mut r10, mut mrr) = (0.0, 0.0, 0.0);
    for q in labelled {
        let ranked = retrieve(q.query);
        let (p, r, rr) = score(&ranked, &q.relevant);
        println!("    {:<18} p@3={:.3} r@10={:.3} rr={:.3}", q.name, p, r, rr);
        p3 += p;
        r10 += r;
        mrr += rr;
    }
    let n = labelled.len() as f64;
    (p3 / n, r10 / n, mrr / n)
}

#[test]
fn ranking_quality_gate() {
    let (_dir, store, labelled) = build_corpus();

    // Populate embeddings for the hybrid path.
    let embedder = HashEmbedder;
    let backlog = store
        .events_without_embedding(embedder.id(), 10_000)
        .unwrap();
    let items: Vec<(i64, Vec<f32>)> = backlog
        .into_iter()
        .map(|(id, content)| (id, embedder.embed(&content)))
        .collect();
    store
        .upsert_embeddings_batch(embedder.id(), &items)
        .unwrap();

    println!("\n=== FTS-only (`search`) ===");
    let (fts_p3, fts_r10, fts_mrr) = evaluate(&labelled, |q| {
        store
            .search(q, None, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect()
    });

    println!("\n=== Hybrid (`search_hybrid`) ===");
    let (hy_p3, hy_r10, hy_mrr) = evaluate(&labelled, |q| {
        store
            .search_hybrid(q, None, 10, &embedder)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect()
    });

    println!("\n=== AVERAGES ===================================");
    println!("            precision@3   recall@10   MRR");
    println!(
        "  FTS         {:.3}        {:.3}      {:.3}",
        fts_p3, fts_r10, fts_mrr
    );
    println!(
        "  Hybrid      {:.3}        {:.3}      {:.3}",
        hy_p3, hy_r10, hy_mrr
    );
    println!("================================================\n");

    // --- Quality floors (regression gate) -------------------------------
    // Floors sit just below observed values so a ranking regression trips
    // the test while normal variation does not.
    assert!(
        fts_p3 >= 0.9,
        "FTS precision@3 regressed: {:.3} < 0.9",
        fts_p3
    );
    assert!(
        fts_r10 >= 0.9,
        "FTS recall@10 regressed: {:.3} < 0.9",
        fts_r10
    );
    assert!(fts_mrr >= 0.95, "FTS MRR regressed: {:.3} < 0.95", fts_mrr);

    assert!(
        hy_p3 >= 0.9,
        "hybrid precision@3 regressed: {:.3} < 0.9",
        hy_p3
    );
    assert!(
        hy_r10 >= 0.9,
        "hybrid recall@10 regressed: {:.3} < 0.9",
        hy_r10
    );
    assert!(hy_mrr >= 0.95, "hybrid MRR regressed: {:.3} < 0.95", hy_mrr);

    // The core contract: hybrid reranking must never rank worse than raw
    // FTS on MRR. If this ever fails it is a real quality signal, not a
    // flaky test — do not weaken it, investigate the reranker.
    assert!(
        hy_mrr >= fts_mrr - 1e-9,
        "hybrid MRR ({:.3}) is worse than FTS MRR ({:.3}) — reranker regression",
        hy_mrr,
        fts_mrr
    );
}
