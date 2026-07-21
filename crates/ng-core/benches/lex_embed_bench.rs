//! Micro-benchmarks for the two pure-CPU primitives on the capture hot path:
//! lexical tag extraction (runs in `ng-hook`, <5ms budget) and the
//! `HashEmbedder` trigram embedding (runs in the daemon enrichment worker).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ng_core::{lex, Embedder, HashEmbedder};

/// Realistic pt-BR + code paragraph, the kind of tool_output that gets
/// tagged and embedded after capture.
const PARAGRAPH: &str = "Corrigir o bug de autenticação: o token JWT em /src/auth/session.rs \
expira antes do refresh, então o endpoint POST /api/login retorna 401. \
A migração sqlite ALTER TABLE events precisa rodar antes do deploy do serviço \
de cache redis com TTL de 300s. Ver docker build layer cache e o runtime tokio.";

fn bench_lex_embed(c: &mut Criterion) {
    c.bench_function("extract_tags", |b| {
        b.iter(|| {
            let tags = lex::extract_tags(black_box(PARAGRAPH));
            black_box(tags);
        });
    });

    let embedder = HashEmbedder;
    c.bench_function("hash_embed", |b| {
        b.iter(|| {
            let vec = embedder.embed(black_box(PARAGRAPH));
            black_box(vec);
        });
    });
}

criterion_group!(benches, bench_lex_embed);
criterion_main!(benches);
