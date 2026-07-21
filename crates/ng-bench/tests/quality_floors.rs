//! Quality floors: the study doubles as a regression gate. These assert that
//! WITH the tool clearly beats WITHOUT on the LEXICAL-OVERLAP class (where a
//! lexical index is supposed to win), that token savings stay large, and — just
//! as importantly — that the SEMANTIC-GAP class stays HONEST: the lexical/hash
//! arms are *expected* to be weak there, and a floor guards that the corpus
//! never quietly regresses into being lexically trivial again (which would make
//! the whole benchmark flatter us).
//!
//! Floors are set conservatively BELOW the measured numbers so ordinary noise
//! doesn't flake CI, but a real regression (a broken embedder, a pruning bug, or
//! a corpus that lost its semantic-gap teeth) trips them.

use ng_bench::run_full_study;
use ng_bench::{ClassResults, StudyResults};

fn study() -> StudyResults {
    let db = std::env::temp_dir().join(format!(
        "ng-bench-test-{}-{:?}.db",
        std::process::id(),
        std::thread::current().id()
    ));
    run_full_study(&db).expect("study runs")
}

fn class<'a>(r: &'a StudyResults, label: &str) -> &'a ClassResults {
    r.by_class
        .iter()
        .find(|c| c.class == label)
        .unwrap_or_else(|| panic!("class {label} present in results"))
}

#[test]
fn baselines_are_the_two_honest_extremes() {
    let r = study();
    // No-memory: nothing surfaced, no cost — the failure floor.
    assert_eq!(r.without_no_memory.accuracy, 0.0);
    assert_eq!(r.without_no_memory.avg_injected_tokens, 0.0);
    // Full-context: the fact is stuffed in, so it "finds" it, but pays the
    // whole prior history and saves nothing vs itself.
    assert_eq!(r.without_full_context.accuracy, 1.0);
    assert_eq!(r.without_full_context.token_savings_pct, 0.0);
    assert!(r.without_full_context.avg_full_context_tokens > 0.0);
}

#[test]
fn lexical_overlap_is_where_fts_matches_full_context() {
    let r = study();
    let lex = class(&r, "lexical-overlap");
    // On the class FTS is built for, retrieval matches full-context recall.
    assert!(
        lex.with_fts.accuracy >= lex.without_full_context.accuracy - f64::EPSILON,
        "fts lexical accuracy {} should match full-context {}",
        lex.with_fts.accuracy,
        lex.without_full_context.accuracy
    );
    // Floor: at least 8/10 lexical tasks surface the gold fact in top-k.
    assert!(
        lex.with_fts.accuracy >= 0.8,
        "fts lexical accuracy floor 0.8, got {}",
        lex.with_fts.accuracy
    );
    assert!(
        lex.with_fts.mrr >= 0.7,
        "fts lexical MRR floor 0.7, got {}",
        lex.with_fts.mrr
    );
}

#[test]
fn semantic_gap_stays_hard_for_lexical_arms() {
    // The anti-flattery gate. The semantic-gap class exists to expose that a
    // lexical index (and a char-trigram HashEmbedder) cannot bridge a true
    // vocabulary gap — recall is FTS-gated, so a paraphrase with no shared
    // token is simply not retrieved. If someone "fixes" the benchmark by
    // sneaking lexical overlap back into these queries, this trips.
    let r = study();
    let lex = class(&r, "lexical-overlap");
    let sem = class(&r, "semantic-gap");
    // Semantic-gap must be measurably harder than lexical-overlap for FTS.
    assert!(
        sem.with_fts.accuracy < lex.with_fts.accuracy,
        "semantic-gap fts accuracy {} must stay below lexical {} (else the gap \
         has no teeth)",
        sem.with_fts.accuracy,
        lex.with_fts.accuracy
    );
    // And there must be at least one honest full-gap miss on FTS.
    assert!(
        sem.with_fts.accuracy < 1.0,
        "semantic-gap must contain at least one fts miss, got accuracy {}",
        sem.with_fts.accuracy
    );
    // The hash embedder cannot rescue a lexical miss (recall is lexical), so it
    // must not magically beat FTS on the semantic class either.
    assert!(
        sem.with_hybrid_hash.accuracy <= sem.with_fts.accuracy + f64::EPSILON,
        "hybrid(hash) semantic accuracy {} should not exceed fts {} — hash \
         cannot bridge a recall gap",
        sem.with_hybrid_hash.accuracy,
        sem.with_fts.accuracy
    );
}

#[test]
fn hybrid_hash_never_regresses_below_fts() {
    // With the embedder-declared rerank weight (hash → 0) hybrid falls back to
    // the FTS ordering and must never lose to it. If a future change
    // reintroduces cosine influence for a weak embedder (or desyncs hybrid's
    // base bm25 from the injection path), these overall floors trip.
    let r = study();
    assert!(
        r.with_hybrid_hash.mrr >= r.with_fts.mrr - 1e-9,
        "hybrid(hash) MRR {} must not regress below fts {}",
        r.with_hybrid_hash.mrr,
        r.with_fts.mrr
    );
    assert!(
        r.with_hybrid_hash.grounded_rate >= r.with_fts.grounded_rate - 1e-9,
        "hybrid(hash) grounding {} must not regress below fts {}",
        r.with_hybrid_hash.grounded_rate,
        r.with_fts.grounded_rate
    );
}

#[test]
fn token_savings_on_delivered_answers_are_large() {
    let r = study();
    // The mem0-comparable headline, made honest: savings counted ONLY on tasks
    // where the gold was actually delivered (a miss injects ~0 tokens and would
    // otherwise fake ~100% savings). Floor 80% (measured is higher).
    let lex = class(&r, "lexical-overlap");
    assert!(
        lex.with_fts.token_savings_pct_on_found >= 80.0,
        "fts lexical savings-on-found floor 80%, got {}",
        lex.with_fts.token_savings_pct_on_found
    );
    assert!(
        lex.with_hybrid_hash.token_savings_pct_on_found >= 80.0,
        "hybrid(hash) lexical savings-on-found floor 80%, got {}",
        lex.with_hybrid_hash.token_savings_pct_on_found
    );
}

#[test]
fn grounding_holds_for_fts_on_lexical() {
    let r = study();
    assert_eq!(r.without_no_memory.grounded_rate, 0.0);
    let lex = class(&r, "lexical-overlap");
    assert!(
        lex.with_fts.grounded_rate >= 0.7,
        "fts lexical grounding floor 0.7, got {}",
        lex.with_fts.grounded_rate
    );
}

#[test]
fn corpus_shape_is_stable() {
    let r = study();
    assert_eq!(r.task_count, 16);
    assert_eq!(r.top_k, 3);
    // 16 est sessions (3 events each = 48) + 6 lexical noise + 2 semantic noise
    // + 16 query-ctx = 72.
    assert_eq!(r.corpus_events, 72);
    // Two classes, with the expected split.
    assert_eq!(class(&r, "lexical-overlap").task_count, 10);
    assert_eq!(class(&r, "semantic-gap").task_count, 6);
}
