//! Latency benchmarks for the wisdom-graph read paths used by the UI's
//! `/api/graph` and by `ng wisdom`: `neighbors` (weighted BFS out to a few
//! hops) and `graph_snapshot` (top-weight nodes + their edges).

mod common;

use common::seed_store_with_graph;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::tempdir;

/// Non-trivial graph: 4k events folded in produces a dense cooccurrence
/// graph (many entities, heavily reinforced edges) without making seeding
/// dominate the run.
const N: usize = 4_000;

fn bench_graph(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let store = seed_store_with_graph(&dir.path().join("ng.db"), N);

    // Pick a real high-weight entity as the neighbors start node instead of
    // hardcoding a name the extractor may not produce.
    let (nodes, _edges) = store.graph_snapshot(None, None, 2, 1).unwrap();
    let start = nodes
        .first()
        .map(|e| e.name.clone())
        .expect("seeded graph has at least one entity");

    c.bench_function("graph_neighbors", |b| {
        b.iter(|| {
            let out = store.neighbors(black_box(&start), None, 2, 20).unwrap();
            black_box(out);
        });
    });

    c.bench_function("graph_snapshot", |b| {
        b.iter(|| {
            let out = store.graph_snapshot(None, None, 2, 50).unwrap();
            black_box(out);
        });
    });
}

criterion_group!(benches, bench_graph);
criterion_main!(benches);
