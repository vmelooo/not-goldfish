//! Full-text, injection-oriented and hybrid (FTS + embedding rerank) search.

use std::collections::HashMap;

use rusqlite::params;

use super::embeddings::decode_vec;
use super::{SearchHit, Store};
use crate::embed::{cosine, Embedder};
use crate::Result;

impl Store {
    /// Full-text search over content + tags, best (lowest bm25) first.
    /// `project`: restrict to a project path; `None` searches globally.
    pub fn search(
        &self,
        query: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let fts_query = sanitize_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT e.id, e.session_id, e.project, e.harness, e.kind,
                    snippet(events_fts, 0, '>>', '<<', ' … ', 24) AS snip,
                    e.tags, e.created_at, bm25(events_fts) AS rank
             FROM events_fts
             JOIN events e ON e.id = events_fts.rowid
             WHERE events_fts MATCH ?1 AND e.hidden_at IS NULL {}
             ORDER BY rank
             LIMIT ?2",
            if project.is_some() {
                "AND e.project = ?3"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<SearchHit> {
            Ok(SearchHit {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project: row.get(2)?,
                harness: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
                tags: row.get(6)?,
                created_at: row.get(7)?,
                rank: row.get(8)?,
            })
        };
        let rows = if let Some(p) = project {
            stmt.query_map(params![fts_query, limit as i64, p], map)?
        } else {
            stmt.query_map(params![fts_query, limit as i64], map)?
        };
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    /// Search used by proactive injection: excludes the current session
    /// (its content is already in the harness context — re-injecting it
    /// would only waste tokens) and structural events without content.
    /// Tags are weighted 2x in ranking. Returns wider snippets.
    pub fn search_for_injection(
        &self,
        query: &str,
        exclude_session: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let fts_query = self.selective_fts_query(query)?;
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.session_id, e.project, e.harness, e.kind,
                    snippet(events_fts, 0, '', '', ' … ', 48) AS snip,
                    e.tags, e.created_at, bm25(events_fts, 1.0, 2.0) AS rank
             FROM events_fts
             JOIN events e ON e.id = events_fts.rowid
             WHERE events_fts MATCH ?1
               AND e.hidden_at IS NULL
               AND e.session_id <> ?2
               AND e.kind IN ('prompt', 'tool_output', 'assistant')
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![fts_query, exclude_session, limit as i64], |row| {
            Ok(SearchHit {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project: row.get(2)?,
                harness: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
                tags: row.get(6)?,
                created_at: row.get(7)?,
                rank: row.get(8)?,
            })
        })?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    /// Build an FTS query keeping only *selective* terms, using dynamic
    /// IDF pruning over the fts5vocab table: a term present in >5% of the
    /// corpus discriminates nothing and forces bm25 to rank a huge
    /// candidate set (measured ~90ms at 100k events). Keeps the 6 rarest
    /// terms. Returns empty when no term is selective — in that case the
    /// caller should stay silent rather than inject noise.
    fn selective_fts_query(&self, query: &str) -> Result<String> {
        let base = sanitize_fts_query(query);
        if base.is_empty() {
            return Ok(String::new());
        }
        // `COUNT(*) FROM events` forces a full-table scan on every search
        // (measured full-scan cost is exactly the 90ms/100k-events figure
        // this function's own doc comment warns about for the FTS side).
        // `MAX(id)` is answered by a single b-tree rightmost-leaf lookup
        // (events.id is the INTEGER PRIMARY KEY, i.e. the rowid itself) —
        // O(log n) instead of O(n). It's a proxy, not an exact count: since
        // this codebase never deletes events, id is monotonically
        // increasing and MAX(id) is an upper bound on the true row count
        // that only diverges from it if row ids were ever non-contiguous
        // (never happens here). That's more than precise enough for a 5%
        // IDF cutoff — being off by a few events shifts max_df by a
        // fraction of a document.
        let total: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| r.get(0))?;
        let max_df = ((total as f64 * 0.05) as i64).max(50);

        let mut stmt = self
            .conn
            .prepare("SELECT COALESCE(SUM(doc), 0) FROM events_fts_vocab WHERE term = ?1")?;
        let mut ranked: Vec<(String, i64)> = Vec::new();
        for quoted in base.split(" OR ") {
            let raw = quoted.trim_matches('"');
            // Look up df for however the *real* tokenizer folds this term
            // (see tokenize_like_index's doc comment for why this replaced
            // a hand-rolled ASCII diacritics table).
            let mut df = 0i64;
            for token in self.tokenize_like_index(raw)? {
                df += stmt.query_row([&token], |r| r.get(0)).unwrap_or(0);
            }
            // df == 0 means the term is absent from the corpus: matching is
            // impossible, skip. df > max_df means it is corpus-common noise.
            if df > 0 && df <= max_df {
                ranked.push((quoted.to_string(), df));
            }
        }
        ranked.sort_by_key(|(_, df)| *df);
        ranked.truncate(6);
        Ok(ranked
            .into_iter()
            .map(|(quoted, _)| quoted)
            .collect::<Vec<_>>()
            .join(" OR "))
    }

    /// Folds `raw` the exact way `events_fts`'s tokenizer
    /// (`unicode61 remove_diacritics 2`) would, by round-tripping it
    /// through a scratch temp-schema FTS5 table configured with the same
    /// tokenizer and reading the resulting token(s) back from its vocab —
    /// instead of a hand-rolled ASCII-only diacritics table.
    ///
    /// The previous approach (`fold_diacritics`) only covered ~20 Latin-1
    /// characters (á, é, ç, ñ, ...). `remove_diacritics 2` folds a much
    /// wider Unicode range — Greek, Cyrillic, and other scripts with
    /// combining marks. A query term outside that hand-rolled table stayed
    /// unfolded, so its `events_fts_vocab` lookup (keyed on the *folded*
    /// form the indexer actually stored) always missed: `df` came back 0,
    /// "absent from corpus", and `selective_fts_query` silently pruned a
    /// term that was actually present — `search_for_injection`/
    /// `search_hybrid` returned nothing for it instead of surfacing a real
    /// match. Delegating to the real tokenizer via this round-trip can
    /// never drift from what indexing actually folds, for any script it
    /// supports, without reimplementing its Unicode tables by hand.
    ///
    /// Returns 0 or 2+ tokens only for edge cases (raw is pure punctuation,
    /// or tokenizes into a compound); the normal case is exactly 1 token
    /// per single word, since `raw` already came from `sanitize_fts_query`
    /// splitting on whitespace.
    ///
    /// A criação das tabelas do probe é lazy e única por conexão (flag
    /// `probe_ready`): esta função roda uma vez por termo do prompt no hot
    /// path de injeção, e re-executar o parse do DDL (`CREATE VIRTUAL TABLE
    /// IF NOT EXISTS`) a cada termo era custo puro. O flag existe porque
    /// tabelas `temp.` morrem com a conexão, não com o processo — cada
    /// `Store` novo precisa recriá-las. DELETE/INSERT/SELECT usam
    /// `prepare_cached` pelo mesmo motivo: mesmo SQL a cada chamada, parse
    /// só na primeira.
    fn tokenize_like_index(&self, raw: &str) -> Result<Vec<String>> {
        if !self.probe_ready.get() {
            self.conn.execute_batch(
                r#"
                CREATE VIRTUAL TABLE IF NOT EXISTS temp.term_probe USING fts5(
                    t, tokenize='unicode61 remove_diacritics 2'
                );
                CREATE VIRTUAL TABLE IF NOT EXISTS temp.term_probe_vocab
                    USING fts5vocab(term_probe, 'row');
                "#,
            )?;
            self.probe_ready.set(true);
        }
        self.conn
            .prepare_cached("DELETE FROM term_probe")?
            .execute([])?;
        self.conn
            .prepare_cached("INSERT INTO term_probe(rowid, t) VALUES (1, ?1)")?
            .execute(params![raw])?;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT term FROM term_probe_vocab")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// FTS candidate retrieval shared by both `search_hybrid` paths.
    ///
    /// Lexical scoring and provenance snippet are kept identical to
    /// `search_for_injection`: same tag-weighted bm25 (`1.0, 2.0`) and the
    /// same wide, marker-free snippet. Hybrid is FTS-plus-rerank, so its
    /// base ordering and the text it surfaces must match the FTS path
    /// exactly — otherwise a zero-weight (weak) embedder could still
    /// diverge from, and underperform, pure FTS. The earlier plain
    /// `bm25(events_fts)` + narrow `>>`/`<<` snippet silently demoted
    /// tag-matched events and truncated provenance below the FTS arm.
    fn fts_candidates(
        &self,
        fts_query: &str,
        project: Option<&str>,
        pool: usize,
    ) -> Result<Vec<SearchHit>> {
        let sql = format!(
            "SELECT e.id, e.session_id, e.project, e.harness, e.kind,
                    snippet(events_fts, 0, '', '', ' … ', 48) AS snip,
                    e.tags, e.created_at, bm25(events_fts, 1.0, 2.0) AS rank
             FROM events_fts
             JOIN events e ON e.id = events_fts.rowid
             WHERE events_fts MATCH ?1 AND e.hidden_at IS NULL {}
             ORDER BY rank
             LIMIT ?2",
            if project.is_some() {
                "AND e.project = ?3"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<SearchHit> {
            Ok(SearchHit {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project: row.get(2)?,
                harness: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
                tags: row.get(6)?,
                created_at: row.get(7)?,
                rank: row.get(8)?,
            })
        };
        let rows = if let Some(p) = project {
            stmt.query_map(params![fts_query, pool as i64, p], map)?
        } else {
            stmt.query_map(params![fts_query, pool as i64], map)?
        };
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row?);
        }
        Ok(candidates)
    }

    /// Cosine similarity of `query_vec` against every stored embedding for
    /// `model`, keyed by event id. Brute-force O(n) scan over the
    /// `embeddings` table (joined to `events` so hidden/project filtering
    /// matches the FTS arm) — acceptable because this only runs at search
    /// time in the daemon/CLI (opt-in real-embedder path), NEVER in the
    /// <5ms hook hot path, and a local store is small (~100k events ⇒ one
    /// sequential read of a few hundred MB worst case, typically far less).
    /// A stored vector whose dimension differs from `query_vec` (older
    /// embedder generation sharing the model id) yields `cosine == 0` via
    /// [`cosine`]'s length guard and is skipped — never cosine over garbage.
    fn cosine_by_event(
        &self,
        query_vec: &[f32],
        model: &str,
        project: Option<&str>,
    ) -> Result<HashMap<i64, f64>> {
        let sql = format!(
            "SELECT em.event_id, em.vec FROM embeddings em
             JOIN events e ON e.id = em.event_id
             WHERE em.model = ?1 AND e.hidden_at IS NULL {}",
            if project.is_some() {
                "AND e.project = ?2"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row<'_>| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?));
        let rows = if let Some(p) = project {
            stmt.query_map(params![model, p], map)?
        } else {
            stmt.query_map(params![model], map)?
        };
        let mut out = HashMap::new();
        for row in rows {
            let (event_id, bytes) = row?;
            let event_vec = decode_vec(&bytes);
            if event_vec.len() != query_vec.len() {
                // Dimension mismatch: stored under the same model id by an
                // incompatible embedder generation. Skip instead of scoring.
                continue;
            }
            out.insert(event_id, cosine(query_vec, &event_vec) as f64);
        }
        Ok(out)
    }

    /// Fetch full [`SearchHit`] rows for ANN-recalled events that FTS never
    /// matched. There is no FTS match to build a `snippet()` from, so the
    /// snippet is a plain content prefix (~ the same width as the FTS arm's
    /// 48-token snippet). `rank` is a placeholder — the caller assigns the
    /// blended score.
    fn fetch_hits_by_id(&self, ids: &[i64]) -> Result<Vec<SearchHit>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT e.id, e.session_id, e.project, e.harness, e.kind,
                    substr(e.content, 1, 300), e.tags, e.created_at
             FROM events e WHERE e.id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let id_params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(id_params.as_slice(), |row| {
            Ok(SearchHit {
                id: row.get(0)?,
                session_id: row.get(1)?,
                project: row.get(2)?,
                harness: row.get(3)?,
                kind: row.get(4)?,
                snippet: row.get(5)?,
                tags: row.get(6)?,
                created_at: row.get(7)?,
                rank: 0.0,
            })
        })?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    /// Hybrid search: FTS recall (same IDF-pruned candidate set as
    /// [`Store::search_for_injection`]) reranked by
    /// `(1 - w) * normalized_bm25 + w * cosine(query, event)`, where
    /// `w = embedder.rerank_weight()`. The weight is embedder-declared so a
    /// weak lexical proxy (the default [`HashEmbedder`], `w = 0`) leaves the
    /// bm25 order untouched and never regresses below pure FTS, while a real
    /// semantic embedder (`w = 0.4`) gets full say. A candidate with no
    /// stored embedding for `embedder.id()` contributes 0 for the cosine
    /// term (score = `(1 - w) * normalized_bm25`) instead of being dropped —
    /// a missing embedding degrades the candidate's ranking rather than
    /// excluding it, since the enrichment worker races the FTS index and most
    /// candidates are unembedded right after capture.
    ///
    /// With a real embedder (`w > 0`) the candidate pool is additionally fed
    /// by **ANN recall**: the top [`ANN_POOL`] events by cosine similarity
    /// over stored embeddings are merged (deduped by event id) into the FTS
    /// candidates *before* the blended rerank. This is what closes the
    /// semantic-gap: a query that paraphrases the stored fact with zero
    /// lexical overlap gets zero FTS candidates, and no reranker can rescue
    /// an empty pool — recall has to come from the embedding side. ANN-only
    /// candidates have no bm25 evidence, so their lexical component is
    /// floored at 0 (`score = w * cosine`): they can only outrank FTS
    /// candidates on semantic strength, never on a fabricated bm25 score.
    /// With `w == 0` (default HashEmbedder) none of this runs — no query
    /// embedding, no embeddings scan — and the result is the pure normalized
    /// bm25 ordering, identical to before ANN recall existed.
    pub fn search_hybrid(
        &self,
        query: &str,
        project: Option<&str>,
        limit: usize,
        embedder: &dyn Embedder,
    ) -> Result<Vec<SearchHit>> {
        const CANDIDATE_POOL: usize = 200;
        /// Máximo de candidatos vindos do recall por embedding. Pequeno em
        /// relação a CANDIDATE_POOL de propósito: ANN-only entra sem
        /// evidência lexical nenhuma, então um pool largo só diluiria o
        /// rerank com vizinhos fracos.
        const ANN_POOL: usize = 50;

        // Embedder-declared rerank weight: a weak embedder (hash) claims
        // near-zero influence so hybrid falls back to bm25 order and never
        // regresses below FTS; a real semantic embedder keeps full weight.
        let rerank_w = embedder.rerank_weight().clamp(0.0, 1.0);
        let fts_query = self.selective_fts_query(query)?;

        // Peso zero (o caso de produção com HashEmbedder): o termo de cosine
        // seria multiplicado por zero de qualquer forma, então pule
        // `embed(query)`, o scan de embeddings e o recall ANN — a ordenação
        // resultante é idêntica ao caminho pré-ANN (bm25 normalizado, sort
        // estável), inclusive nos retornos vazios.
        if rerank_w == 0.0 {
            if fts_query.is_empty() {
                return Ok(Vec::new());
            }
            let candidates = self.fts_candidates(&fts_query, project, CANDIDATE_POOL)?;
            if candidates.is_empty() {
                return Ok(Vec::new());
            }
            let normalize_bm25 = bm25_normalizer(&candidates);
            let mut hits: Vec<SearchHit> = candidates
                .into_iter()
                .map(|mut hit| {
                    hit.rank = normalize_bm25(hit.rank);
                    hit
                })
                .collect();
            hits.sort_by(|a, b| {
                b.rank
                    .partial_cmp(&a.rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            hits.truncate(limit);
            return Ok(hits);
        }

        // Real-embedder path: FTS candidates (possibly none — the
        // semantic-gap case) + ANN recall, then one blended rerank.
        let candidates = if fts_query.is_empty() {
            Vec::new()
        } else {
            self.fts_candidates(&fts_query, project, CANDIDATE_POOL)?
        };
        let normalize_bm25 = bm25_normalizer(&candidates);

        let query_vec = embedder.embed(query);
        // One O(n) scan yields cosines for BOTH sides: the FTS candidates'
        // rerank term (replacing the old batched `IN (...)` lookup) and the
        // ANN recall pool.
        let cosines = self.cosine_by_event(&query_vec, embedder.id(), project)?;

        let fts_ids: std::collections::HashSet<i64> = candidates.iter().map(|h| h.id).collect();
        // Top-ANN_POOL events by cosine that FTS did not already recall.
        // `cos > 0.0` also drops dimension-degenerate/zero-vector rows.
        let mut ann_ranked: Vec<(i64, f64)> = cosines
            .iter()
            .filter(|(id, cos)| !fts_ids.contains(id) && **cos > 0.0)
            .map(|(id, cos)| (*id, *cos))
            .collect();
        ann_ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ann_ranked.truncate(ANN_POOL);
        let ann_ids: Vec<i64> = ann_ranked.iter().map(|(id, _)| *id).collect();
        let ann_hits = self.fetch_hits_by_id(&ann_ids)?;

        let mut hits: Vec<SearchHit> = candidates
            .into_iter()
            .map(|mut hit| {
                let norm_bm25 = normalize_bm25(hit.rank);
                let sim = cosines.get(&hit.id).copied().unwrap_or(0.0);
                hit.rank = (1.0 - rerank_w) * norm_bm25 + rerank_w * sim;
                hit
            })
            // ANN-only: bm25 component floored at 0 (no lexical evidence).
            .chain(ann_hits.into_iter().map(|mut hit| {
                hit.rank = rerank_w * cosines.get(&hit.id).copied().unwrap_or(0.0);
                hit
            }))
            .collect();

        hits.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

/// bm25 is lower-is-better and unbounded; min-max normalize into
/// [0, 1] higher-is-better so it can be linearly combined with cosine.
/// Empty candidate set yields a closure that is never called.
fn bm25_normalizer(candidates: &[SearchHit]) -> impl Fn(f64) -> f64 {
    let min_bm25 = candidates
        .iter()
        .map(|h| h.rank)
        .fold(f64::INFINITY, f64::min);
    let max_bm25 = candidates
        .iter()
        .map(|h| h.rank)
        .fold(f64::NEG_INFINITY, f64::max);
    let bm25_span = max_bm25 - min_bm25;
    move |raw: f64| -> f64 {
        if bm25_span > 0.0 {
            1.0 - (raw - min_bm25) / bm25_span
        } else {
            1.0
        }
    }
}

/// FTS5 treats many characters as syntax; user queries are data, not syntax.
/// Quote each token so `bug: can't-repro` never becomes a parse error, and
/// join with OR so partial matches still surface (bm25 ranks them below
/// full matches anyway).
///
/// Stopwords are dropped BEFORE building the query: an OR over "com"/"de"
/// matches half the database, which both poisons ranking (irrelevant hits
/// outscore real ones) and forces bm25 to rank tens of thousands of
/// candidates (~90ms at 100k events vs ~2ms without stopwords).
fn sanitize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|token| {
            let lower = token.to_lowercase();
            token.len() >= 2 && !crate::lex::STOPWORDS.contains(&lower.as_str())
        })
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}
