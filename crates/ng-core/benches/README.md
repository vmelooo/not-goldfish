# ng-core benchmarks & search-quality gate

Two kinds of measurement live here:

- **Latency benchmarks** (criterion, `benches/*.rs`) — how fast the read
  paths are.
- **Ranking-quality gate** (`tests/relevance.rs`) — how *good* the ranking
  is. This one is a normal `#[test]` and runs in CI.

Both are fully deterministic: fixed content, fixed `created_at` epochs, no
`rand`, no wall clock. A run is reproducible across machines.

## Run the latency benchmarks

```bash
cargo bench -p ng-core
```

Quick smoke run (compiles + one measurement pass, seconds instead of
minutes):

```bash
cargo bench -p ng-core -- --warm-up-time 1 --measurement-time 2
```

Benches:

| Bench | Measures |
|---|---|
| `search_bench` | `search` (FTS) and `search_hybrid` at 1k and 10k events |
| `graph_bench` | `neighbors` and `graph_snapshot` on a ~4k-event graph |
| `lex_embed_bench` | `lex::extract_tags` and `HashEmbedder::embed` on a realistic paragraph |

HTML reports land in `target/criterion/`.

## Run the search-quality gate

```bash
cargo test -p ng-core --test relevance -- --nocapture
```

Builds a deterministic synthetic corpus (five well-separated topics, each
with known-relevant docs plus hard negatives and generic distractors),
then prints precision@3 / recall@10 / MRR for both the FTS-only path and
the hybrid path and asserts quality floors. See the module doc comment in
`tests/relevance.rs` for metric definitions and the reasoning behind the
floors.
