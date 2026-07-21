//! Semantic embeddings for hybrid search.
//!
//! [`Embedder`] is intentionally a trait: the default [`HashEmbedder`] is a
//! pure-Rust, zero-dependency feature-hashing embedding (char-trigrams →
//! signed hash buckets) good enough to rerank an FTS candidate set. A real
//! ONNX/sentence-transformer embedder can be plugged in later behind the
//! same trait without touching the store or the daemon worker.

/// Produces a fixed-dimension vector for a piece of text.
pub trait Embedder {
    fn embed(&self, text: &str) -> Vec<f32>;
    fn dim(&self) -> usize;
    /// Stable identifier stored alongside vectors so multiple embedder
    /// generations can coexist in the same database.
    fn id(&self) -> &str;

    /// How much the cosine term is allowed to influence hybrid reranking,
    /// in `[0, 1]`. `search_hybrid` blends `(1 - w) * bm25_norm + w * cosine`,
    /// so this is the embedder's own declaration of how trustworthy its
    /// semantic signal is relative to lexical (bm25) evidence.
    ///
    /// The default (`0.4`) suits a real semantic embedder, whose vectors
    /// carry enough meaning to overrule bm25 when the two disagree. A weak
    /// embedder (see [`HashEmbedder`]) overrides this downward so its noisy
    /// signal can only break ties, never demote an exact lexical match that
    /// FTS already ranked correctly. Keeping it a trait method (with a
    /// default) means the rule is a general property of the embedder, not a
    /// call-site special case: no existing caller changes, and every future
    /// embedder picks its own weight.
    fn rerank_weight(&self) -> f64 {
        0.4
    }
}

const HASH_DIM: usize = 256;

/// Feature-hashed char-trigram embedding. Deterministic across processes
/// (no `RandomState`/`DefaultHasher` — those seed randomly per-process,
/// which would make stored vectors incomparable across runs).
pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0f32; HASH_DIM];
        let normalized = fold_diacritics(&text.to_lowercase());
        let chars: Vec<char> = normalized.chars().collect();

        // Fewer than 3 chars: no trigram window fits, fall back to the
        // whole (short) text as a single pseudo-trigram so short queries
        // still produce a non-zero vector.
        if chars.len() < 3 {
            if chars.is_empty() {
                return vec;
            }
            hash_into(&normalized, &mut vec);
        } else {
            for window in chars.windows(3) {
                let trigram: String = window.iter().collect();
                hash_into(&trigram, &mut vec);
            }
        }

        l2_normalize(&mut vec);
        vec
    }

    fn dim(&self) -> usize {
        HASH_DIM
    }

    fn id(&self) -> &str {
        "hash3-256-v1"
    }

    /// Char-trigram feature hashing is a *lexical* similarity proxy, not a
    /// semantic one: it mostly re-measures the surface overlap that bm25
    /// already scores, plus hash-collision noise. Given full rerank weight it
    /// actively hurts — the noise demotes exact matches FTS ranked correctly
    /// (measured: hybrid MRR 0.85 vs FTS 0.90 on the ng-bench corpus). So the
    /// hash embedder claims (near-)zero influence: hybrid falls back to the
    /// bm25 ordering and is never worse than pure FTS, while a real semantic
    /// embedder keeps the full default weight.
    fn rerank_weight(&self) -> f64 {
        0.0
    }
}

/// Hash one token into the accumulator: bucket index from one half of the
/// hash, sign from the other half, so index and sign don't collapse to the
/// same bit.
fn hash_into(token: &str, vec: &mut [f32]) {
    let hash = fnv1a(token.as_bytes());
    let idx = (hash % HASH_DIM as u32) as usize;
    let sign = if (hash / HASH_DIM as u32) & 1 == 1 {
        1.0
    } else {
        -1.0
    };
    vec[idx] += sign;
}

/// Manual FNV-1a: deterministic across processes and platforms, unlike
/// `std::collections::hash_map::DefaultHasher` (randomly seeded).
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}

/// ASCII-fold the latin-1 diacritics relevant to pt-BR/es/fr — mirrors the
/// FTS tokenizer's `remove_diacritics` so query and index text normalize
/// the same way.
fn fold_diacritics(term: &str) -> String {
    term.chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// Fábrica do embedder padrão do processo. Com a feature `model2vec` ligada
/// E um modelo disponível, retorna um `Model2VecEmbedder`; caso contrário
/// (feature desligada, modelo ausente ou falha de carga) cai no
/// [`HashEmbedder`] zero-dependência. Nunca entra em pânico — perder o
/// embedder semântico degrada a qualidade da busca, não quebra o processo.
///
/// Callers atuais (worker de enrich do ngd, ng-cli, ng-hook) continuam
/// construindo `HashEmbedder` diretamente; esta fábrica existe para uma
/// migração futura e não muda nada quando a feature está desligada.
pub fn default_embedder() -> Box<dyn Embedder> {
    #[cfg(feature = "model2vec")]
    {
        match crate::embed_model2vec::Model2VecEmbedder::from_env() {
            Ok(embedder) => return Box::new(embedder),
            Err(err) => {
                eprintln!("[ng-core] model2vec indisponível ({err}); usando HashEmbedder");
            }
        }
    }
    Box::new(HashEmbedder)
}

/// Cosine similarity. Returns 0.0 for a zero-norm vector rather than NaN —
/// a degenerate embedding should rank as "unrelated", not poison the sort.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_across_calls() {
        let e = HashEmbedder;
        let v1 = e.embed("bug de autenticacao no login");
        let v2 = e.embed("bug de autenticacao no login");
        assert_eq!(v1, v2);
    }

    #[test]
    fn correct_dimension() {
        let e = HashEmbedder;
        assert_eq!(e.embed("qualquer texto").len(), HASH_DIM);
        assert_eq!(e.dim(), HASH_DIM);
    }

    #[test]
    fn empty_text_does_not_explode() {
        let e = HashEmbedder;
        let v = e.embed("");
        assert_eq!(v.len(), HASH_DIM);
        assert!(v.iter().all(|x| *x == 0.0));
        assert_eq!(cosine(&v, &v), 0.0);
    }

    #[test]
    fn similar_texts_score_higher_than_disparate_ones() {
        let e = HashEmbedder;
        let a = e.embed("bug de autenticacao no login");
        let b = e.embed("problema de auth no login");
        let c = e.embed("receita de bolo de chocolate");

        let sim_close = cosine(&a, &b);
        let sim_far = cosine(&a, &c);
        assert!(
            sim_close > sim_far,
            "esperado {sim_close} > {sim_far} (textos similares vs. díspares)"
        );
    }

    #[test]
    fn cosine_dim_mismatch_is_zero() {
        // Vetores de dimensões diferentes (ex.: HashEmbedder 256-dim vs. um
        // model2vec de outra dimensão) nunca devem produzir cosseno de lixo:
        // a guarda de comprimento retorna 0.0 em vez de comparar bytes
        // desalinhados.
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn diacritics_and_case_normalize_the_same() {
        let e = HashEmbedder;
        let a = e.embed("configuração do serviço");
        let b = e.embed("CONFIGURACAO DO SERVICO");
        assert_eq!(a, b);
    }
}
