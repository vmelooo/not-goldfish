//! Latency benchmarks for the two read paths that sit in front of every
//! injection: FTS-only `search` and the reranked `search_hybrid`. Seeded at
//! two corpus sizes so the numbers document how ranking cost scales.

mod common;

use common::{seed_store, seed_store_with_embeddings};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use ng_core::HashEmbedder;
use tempfile::tempdir;

/// pt-BR + code query hitting several topics in the seeded corpus.
const QUERY: &str = "autenticação login token cache redis docker build sqlite";

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_fts");
    for &n in &[1_000usize, 10_000] {
        // Seeding is outside the measured closure: a store per size, reused
        // across iterations.
        let dir = tempdir().unwrap();
        let store = seed_store(&dir.path().join("ng.db"), n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let hits = store.search(black_box(QUERY), None, 10).unwrap();
                black_box(hits);
            });
        });
    }
    group.finish();
}

fn bench_search_hybrid(c: &mut Criterion) {
    let embedder = HashEmbedder;
    let mut group = c.benchmark_group("search_hybrid");
    for &n in &[1_000usize, 10_000] {
        let dir = tempdir().unwrap();
        let store = seed_store_with_embeddings(&dir.path().join("ng.db"), n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let hits = store
                    .search_hybrid(black_box(QUERY), None, 10, &embedder)
                    .unwrap();
                black_box(hits);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_search, bench_search_hybrid);
criterion_main!(benches);
