//! The measurement harness: seed the corpus into a real ng-core [`Store`],
//! run each arm, and compute retrieval + token-cost metrics.
//!
//! Metric definitions (all reported per-task then averaged):
//! - **accuracy**: fraction of tasks where at least one gold event lands in
//!   the top-k injection set. WITHOUT has no injection → 0 by construction.
//! - **recall@k**: retrieved gold / total gold, per task.
//! - **precision@k**: retrieved gold / returned hits.
//! - **MRR**: 1 / rank of the first gold hit (0 if none).
//! - **injected_tokens**: sum of snippet token estimates actually surfaced
//!   (bytes/4, matching `Event::tokens_est`). This is the *bounded* cost the
//!   tool adds to the prompt.
//! - **replay_tokens**: total tokens of the establishing session a memoryless
//!   agent would re-read to recover the fact (the WITHOUT counterfactual).
//! - **token_savings_pct**: (replay - injected) / replay — comparable to
//!   mem0's LoCoMo "~90% fewer tokens vs full-context" figure.
//! - **grounded**: a retrieved snippet contains the task's needle → the answer
//!   is supported by provenance (hallucination proxy).

use std::collections::HashMap;

use ng_core::{Embedder, SearchHit, Store};
use serde::Serialize;

use crate::corpus::{build_corpus, Corpus, Task, TaskClass, BASE_TS, PROJECT};

/// Injection budget: how many memories the tool is allowed to surface.
pub const TOP_K: usize = 3;

/// bytes/4 token estimate — the exact heuristic `Event::tokens_est` uses, so
/// injected and replay costs are measured on the same ruler.
fn tokens_est(s: &str) -> i64 {
    (s.len() / 4) as i64
}

/// ASCII-fold + lowercase for a diacritics-insensitive needle check, mirroring
/// how the store normalizes text.
fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

/// Per-task, per-arm measurement.
#[derive(Clone)]
pub struct TaskMetric {
    pub found: bool,
    pub mrr: f64,
    pub recall: f64,
    pub precision: f64,
    /// Tokens this arm actually adds to the prompt.
    pub injected_tokens: i64,
    /// Full-context baseline: the whole prior history a memoryless agent would
    /// stuff (it doesn't know which session holds the fact). mem0-comparable.
    pub full_context_tokens: i64,
    /// Oracle baseline: only the establishing session, i.e. the best case if
    /// the agent somehow already knew exactly where to look.
    pub oracle_tokens: i64,
    pub grounded: bool,
}

/// Averaged metrics for one arm across all tasks.
#[derive(Serialize, Clone)]
pub struct ArmSummary {
    pub name: String,
    /// How many tasks this summary averages over (whole corpus, or one class).
    pub task_count: usize,
    pub accuracy: f64,
    pub recall_at_k: f64,
    pub mrr: f64,
    pub precision_at_k: f64,
    pub avg_injected_tokens: f64,
    pub avg_full_context_tokens: f64,
    pub avg_oracle_tokens: f64,
    /// Savings vs the full-context baseline (mem0's headline metric).
    pub token_savings_pct: f64,
    /// Savings vs the oracle baseline — the honest worst case: if the agent
    /// already knew the exact session, retrieval saves little (or costs more).
    pub token_savings_vs_oracle_pct: f64,
    pub grounded_rate: f64,
    /// Fraction of tasks where the gold fact was actually delivered (== accuracy
    /// here, surfaced explicitly because the savings-on-found figures below are
    /// only meaningful in proportion to it).
    pub found_rate: f64,
    /// Token savings vs full-context counting ONLY tasks where the gold was
    /// found. A miss injects ~0 tokens and would otherwise inflate "savings" to
    /// ~100% — a failure masquerading as a token win. This figure refuses that:
    /// it is the savings on queries the tool actually answered. 0.0 when the arm
    /// found nothing.
    pub token_savings_pct_on_found: f64,
}

/// Seed every corpus event and return key → row-id map. Empty keys (fillers)
/// are not tracked.
pub fn seed(store: &Store, corpus: &Corpus) -> ng_core::Result<HashMap<String, i64>> {
    let mut map = HashMap::new();
    for ev in &corpus.events {
        let event = ng_core::Event {
            session_id: ev.session_id.to_string(),
            project: PROJECT.to_string(),
            harness: "claude-code".to_string(),
            kind: ev.kind.to_string(),
            content: ev.content.to_string(),
            tags: ev.tags.to_string(),
            meta: None,
            created_at: BASE_TS + ev.ts_offset,
        };
        let id = store.insert_event(&event)?;
        if !ev.key.is_empty() {
            map.insert(ev.key.to_string(), id);
        }
    }
    Ok(map)
}

/// Populate the embeddings table so `search_hybrid`'s cosine term is live
/// (otherwise it degenerates to normalized bm25). The store assigns ids
/// sequentially in insert order starting at 1, and [`seed`] inserts exactly
/// `corpus.events` in order, so `id == index + 1`.
pub fn embed_corpus(
    store: &Store,
    corpus: &Corpus,
    embedder: &dyn Embedder,
) -> ng_core::Result<()> {
    // The store assigns ids sequentially in insert order starting at 1, and we
    // insert exactly `corpus.events` in order, so id == index + 1.
    for (idx, ev) in corpus.events.iter().enumerate() {
        if matches!(ev.kind, "prompt" | "tool_output" | "assistant") {
            let id = (idx + 1) as i64;
            store.upsert_embedding(id, embedder.id(), &embedder.embed(ev.content))?;
        }
    }
    Ok(())
}

fn gold_ids(task: &Task, map: &HashMap<String, i64>) -> Vec<i64> {
    task.gold_keys
        .iter()
        .filter_map(|k| map.get(*k).copied())
        .collect()
}

/// Oracle baseline: tokens of the single establishing session.
fn oracle_tokens(corpus: &Corpus, task: &Task) -> i64 {
    corpus
        .events
        .iter()
        .filter(|e| e.session_id == task.replay_session)
        .map(|e| tokens_est(e.content))
        .sum()
}

/// Full-context baseline: every prior event the agent would stuff when it has
/// no memory index and no idea which session holds the fact — the whole corpus
/// minus its own (already-in-window) query session.
fn full_context_tokens(corpus: &Corpus, task: &Task) -> i64 {
    corpus
        .events
        .iter()
        .filter(|e| e.session_id != task.query_session)
        .map(|e| tokens_est(e.content))
        .sum()
}

/// Score a ranked hit list against the gold set for one task.
fn eval_hits(
    hits: &[SearchHit],
    gold: &[i64],
    needle: &str,
    full_ctx: i64,
    oracle: i64,
) -> TaskMetric {
    let topk = &hits[..hits.len().min(TOP_K)];
    let mut first_gold_rank: Option<usize> = None;
    let mut retrieved = 0usize;
    for (i, h) in topk.iter().enumerate() {
        if gold.contains(&h.id) {
            retrieved += 1;
            if first_gold_rank.is_none() {
                first_gold_rank = Some(i + 1);
            }
        }
    }
    let found = retrieved > 0;
    let mrr = first_gold_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0);
    let recall = if gold.is_empty() {
        0.0
    } else {
        retrieved as f64 / gold.len() as f64
    };
    let precision = if topk.is_empty() {
        0.0
    } else {
        retrieved as f64 / topk.len() as f64
    };
    let needle_f = fold(needle);
    let grounded = topk.iter().any(|h| fold(&h.snippet).contains(&needle_f));
    let injected_tokens = topk.iter().map(|h| tokens_est(&h.snippet)).sum();
    TaskMetric {
        found,
        mrr,
        recall,
        precision,
        injected_tokens,
        full_context_tokens: full_ctx,
        oracle_tokens: oracle,
        grounded,
    }
}

/// WITHOUT — no memory: the agent has neither the fact nor a way to recover it
/// cheaply, so it hallucinates or fails. accuracy 0, cost 0. The floor.
pub fn run_without_no_memory(corpus: &Corpus) -> Vec<TaskMetric> {
    corpus
        .tasks
        .iter()
        .map(|t| TaskMetric {
            found: false,
            mrr: 0.0,
            recall: 0.0,
            precision: 0.0,
            injected_tokens: 0,
            full_context_tokens: full_context_tokens(corpus, t),
            oracle_tokens: oracle_tokens(corpus, t),
            grounded: false,
        })
        .collect()
}

/// WITHOUT — full-context replay: the mem0-comparable baseline. The fact IS in
/// the prompt (everything is stuffed), so retrieval trivially succeeds, but at
/// the cost of the entire prior history. This is what the tool undercuts.
pub fn run_without_full_context(corpus: &Corpus) -> Vec<TaskMetric> {
    corpus
        .tasks
        .iter()
        .map(|t| {
            let full = full_context_tokens(corpus, t);
            TaskMetric {
                found: true,
                mrr: 1.0,
                recall: 1.0,
                precision: 0.0, // one relevant fact buried in the whole dump
                injected_tokens: full,
                full_context_tokens: full,
                oracle_tokens: oracle_tokens(corpus, t),
                grounded: true,
            }
        })
        .collect()
}

/// WITH (fts injection): the real proactive-injection path,
/// `search_for_injection` (IDF-pruned FTS, tags weighted 2x).
pub fn run_fts(
    store: &Store,
    corpus: &Corpus,
    map: &HashMap<String, i64>,
) -> ng_core::Result<Vec<TaskMetric>> {
    let mut out = Vec::new();
    for t in &corpus.tasks {
        let hits = store.search_for_injection(t.query, t.query_session, TOP_K)?;
        let gold = gold_ids(t, map);
        out.push(eval_hits(
            &hits,
            &gold,
            t.needle,
            full_context_tokens(corpus, t),
            oracle_tokens(corpus, t),
        ));
    }
    Ok(out)
}

/// WITH (hybrid): `search_hybrid` — FTS recall (plus ANN recall over stored
/// vectors when the embedder declares `rerank_weight > 0`) reranked by
/// `(1-w)*bm25_norm + w*cosine` against the given embedder's stored vectors.
pub fn run_hybrid(
    store: &Store,
    corpus: &Corpus,
    map: &HashMap<String, i64>,
    embedder: &dyn Embedder,
) -> ng_core::Result<Vec<TaskMetric>> {
    let mut out = Vec::new();
    for t in &corpus.tasks {
        let hits = store.search_hybrid(t.query, None, TOP_K, embedder)?;
        let gold = gold_ids(t, map);
        out.push(eval_hits(
            &hits,
            &gold,
            t.needle,
            full_context_tokens(corpus, t),
            oracle_tokens(corpus, t),
        ));
    }
    Ok(out)
}

/// Average a list of per-task metrics into an arm summary.
pub fn summarize(name: &str, metrics: &[TaskMetric]) -> ArmSummary {
    let n = metrics.len().max(1) as f64;
    let mean = |f: &dyn Fn(&TaskMetric) -> f64| metrics.iter().map(f).sum::<f64>() / n;
    let avg_injected = mean(&|m| m.injected_tokens as f64);
    let avg_full = mean(&|m| m.full_context_tokens as f64);
    let avg_oracle = mean(&|m| m.oracle_tokens as f64);
    let savings = |base: f64| {
        if base > 0.0 {
            (base - avg_injected) / base * 100.0
        } else {
            0.0
        }
    };
    // Savings restricted to tasks the arm actually answered (gold delivered).
    // A miss injects ~0 tokens; averaging it in would fake a token win out of a
    // retrieval failure, so those tasks are excluded from this figure.
    let found: Vec<&TaskMetric> = metrics.iter().filter(|m| m.found).collect();
    let token_savings_pct_on_found = if found.is_empty() {
        0.0
    } else {
        let fn_ = found.len() as f64;
        let inj = found.iter().map(|m| m.injected_tokens as f64).sum::<f64>() / fn_;
        let full = found
            .iter()
            .map(|m| m.full_context_tokens as f64)
            .sum::<f64>()
            / fn_;
        if full > 0.0 {
            (full - inj) / full * 100.0
        } else {
            0.0
        }
    };
    ArmSummary {
        name: name.to_string(),
        task_count: metrics.len(),
        accuracy: mean(&|m| if m.found { 1.0 } else { 0.0 }),
        recall_at_k: mean(&|m| m.recall),
        mrr: mean(&|m| m.mrr),
        precision_at_k: mean(&|m| m.precision),
        avg_injected_tokens: avg_injected,
        avg_full_context_tokens: avg_full,
        avg_oracle_tokens: avg_oracle,
        token_savings_pct: savings(avg_full),
        token_savings_vs_oracle_pct: savings(avg_oracle),
        grounded_rate: mean(&|m| if m.grounded { 1.0 } else { 0.0 }),
        found_rate: mean(&|m| if m.found { 1.0 } else { 0.0 }),
        token_savings_pct_on_found,
    }
}

/// Summarize only the tasks of a given class (by task index), so lexical-overlap
/// and semantic-gap results are reported separately — averaging them would hide
/// exactly where lexical retrieval is strong and where it is blind.
pub fn summarize_class(
    name: &str,
    corpus: &Corpus,
    metrics: &[TaskMetric],
    class: TaskClass,
) -> ArmSummary {
    let subset: Vec<TaskMetric> = corpus
        .tasks
        .iter()
        .zip(metrics.iter())
        .filter(|(t, _)| t.class == class)
        .map(|(_, m)| m.clone())
        .collect();
    summarize(name, &subset)
}

/// Per-task row for the machine-readable output and the "where worse" analysis.
#[derive(Serialize)]
pub struct PerTaskRow {
    pub task: String,
    pub class: String,
    pub full_context_tokens: i64,
    pub oracle_tokens: i64,
    pub fts_found: bool,
    pub fts_mrr: f64,
    pub fts_injected_tokens: i64,
    pub hybrid_hash_found: bool,
    pub hybrid_hash_mrr: f64,
    pub hybrid_m2v_found: Option<bool>,
    pub hybrid_m2v_mrr: Option<f64>,
}

/// The full study result: metadata + one summary per arm + per-task detail.
#[derive(Serialize)]
pub struct StudyResults {
    pub top_k: usize,
    pub task_count: usize,
    pub corpus_events: usize,
    pub embedder_hash_id: String,
    pub embedder_m2v_id: Option<String>,
    pub without_no_memory: ArmSummary,
    pub without_full_context: ArmSummary,
    pub with_fts: ArmSummary,
    pub with_hybrid_hash: ArmSummary,
    pub with_hybrid_m2v: Option<ArmSummary>,
    /// The same arms broken down by task class (lexical-overlap vs
    /// semantic-gap). This is the fairness-critical view: the overall averages
    /// above blend the two and hide where lexical retrieval fails.
    pub by_class: Vec<ClassResults>,
    pub per_task: Vec<PerTaskRow>,
    /// mem0's published LoCoMo reference numbers we compare against.
    pub mem0_reference: Mem0Reference,
}

/// Per-class breakdown of every arm. Semantic-gap is where the current
/// lexical/hash arms are expected to be weak — kept separate on purpose.
#[derive(Serialize)]
pub struct ClassResults {
    pub class: String,
    pub task_count: usize,
    pub without_full_context: ArmSummary,
    pub with_fts: ArmSummary,
    pub with_hybrid_hash: ArmSummary,
    pub with_hybrid_m2v: Option<ArmSummary>,
}

#[derive(Serialize)]
pub struct Mem0Reference {
    pub token_savings_pct_vs_full_context: f64,
    pub accuracy_mem0: f64,
    pub accuracy_full_context: f64,
    pub source: String,
}

/// Optional semantic arm. Returns (summary, embedder-id, per-task metrics) when
/// the `model2vec` feature is on AND a model loads from NG_EMBED_MODEL.
#[cfg(feature = "model2vec")]
fn run_m2v_arm(
    store: &Store,
    corpus: &Corpus,
    map: &HashMap<String, i64>,
) -> ng_core::Result<Option<(ArmSummary, String, Vec<TaskMetric>)>> {
    use ng_core::embed_model2vec::Model2VecEmbedder;
    match Model2VecEmbedder::from_env() {
        Ok(embedder) => {
            embed_corpus(store, corpus, &embedder)?;
            let metrics = run_hybrid(store, corpus, map, &embedder)?;
            let summary = summarize("WITH hybrid (model2vec)", &metrics);
            Ok(Some((summary, embedder.id().to_string(), metrics)))
        }
        Err(err) => {
            eprintln!("[ng-bench] model2vec indisponível ({err}); pulando arm semântico");
            Ok(None)
        }
    }
}

#[cfg(not(feature = "model2vec"))]
fn run_m2v_arm(
    _store: &Store,
    _corpus: &Corpus,
    _map: &HashMap<String, i64>,
) -> ng_core::Result<Option<(ArmSummary, String, Vec<TaskMetric>)>> {
    Ok(None)
}

/// Run the complete study against a fresh store at `db_path`. Rebuilds the
/// corpus, seeds it, runs every arm, and returns aggregated results.
pub fn run_full_study(db_path: &std::path::Path) -> anyhow::Result<StudyResults> {
    let _ = std::fs::remove_file(db_path);
    let store = Store::open(db_path)?;
    let corpus = build_corpus();
    let map = seed(&store, &corpus)?;

    let hash = ng_core::HashEmbedder;
    embed_corpus(&store, &corpus, &hash)?;

    let no_mem_m = run_without_no_memory(&corpus);
    let full_ctx_m = run_without_full_context(&corpus);
    let fts_m = run_fts(&store, &corpus, &map)?;
    let hybrid_hash_m = run_hybrid(&store, &corpus, &map, &hash)?;
    let m2v = run_m2v_arm(&store, &corpus, &map)?;

    let per_task = corpus
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| PerTaskRow {
            task: t.name.to_string(),
            class: t.class.label().to_string(),
            full_context_tokens: full_ctx_m[i].full_context_tokens,
            oracle_tokens: full_ctx_m[i].oracle_tokens,
            fts_found: fts_m[i].found,
            fts_mrr: fts_m[i].mrr,
            fts_injected_tokens: fts_m[i].injected_tokens,
            hybrid_hash_found: hybrid_hash_m[i].found,
            hybrid_hash_mrr: hybrid_hash_m[i].mrr,
            hybrid_m2v_found: m2v.as_ref().map(|(_, _, mm)| mm[i].found),
            hybrid_m2v_mrr: m2v.as_ref().map(|(_, _, mm)| mm[i].mrr),
        })
        .collect();

    let by_class = [TaskClass::LexicalOverlap, TaskClass::SemanticGap]
        .into_iter()
        .map(|class| {
            let n = corpus.tasks.iter().filter(|t| t.class == class).count();
            ClassResults {
                class: class.label().to_string(),
                task_count: n,
                without_full_context: summarize_class(
                    "WITHOUT - full-context replay",
                    &corpus,
                    &full_ctx_m,
                    class,
                ),
                with_fts: summarize_class("WITH fts injection", &corpus, &fts_m, class),
                with_hybrid_hash: summarize_class(
                    "WITH hybrid (hash)",
                    &corpus,
                    &hybrid_hash_m,
                    class,
                ),
                with_hybrid_m2v: m2v.as_ref().map(|(_, _, mm)| {
                    summarize_class("WITH hybrid (model2vec)", &corpus, mm, class)
                }),
            }
        })
        .collect();

    let results = StudyResults {
        top_k: TOP_K,
        task_count: corpus.tasks.len(),
        corpus_events: corpus.events.len(),
        embedder_hash_id: hash.id().to_string(),
        embedder_m2v_id: m2v.as_ref().map(|(_, id, _)| id.clone()),
        without_no_memory: summarize("WITHOUT - no memory", &no_mem_m),
        without_full_context: summarize("WITHOUT - full-context replay", &full_ctx_m),
        with_fts: summarize("WITH fts injection", &fts_m),
        with_hybrid_hash: summarize("WITH hybrid (hash)", &hybrid_hash_m),
        with_hybrid_m2v: m2v.as_ref().map(|(s, _, _)| s.clone()),
        by_class,
        per_task,
        mem0_reference: Mem0Reference {
            token_savings_pct_vs_full_context: 90.0,
            accuracy_mem0: 61.4,
            accuracy_full_context: 72.9,
            source: "mem0 LoCoMo (arXiv 2504.19413; mem0.ai benchmarks 2026)".to_string(),
        },
    };
    let _ = std::fs::remove_file(db_path);
    Ok(results)
}
