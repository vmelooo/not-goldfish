use std::collections::HashSet;

use rusqlite::{params, Connection};

use super::embeddings::{decode_vec, encode_vec};
use super::*;
use crate::embed::Embedder;
use crate::event::Event;
use crate::gain::GainRecord;

fn mem_store() -> Store {
    Store::init(Connection::open_in_memory().unwrap()).unwrap()
}

fn ev(kind: &str, content: &str, tags: &str) -> Event {
    Event {
        session_id: "s1".into(),
        project: "/tmp/proj".into(),
        harness: "claude-code".into(),
        kind: kind.into(),
        content: content.into(),
        tags: tags.into(),
        meta: None,
        created_at: 1_700_000_000,
    }
}

fn gain(kind: &str, project: &str, tokens: i64, items: i64, created_at: i64) -> GainRecord {
    GainRecord {
        kind: kind.into(),
        session_id: "s1".into(),
        project: project.into(),
        tokens,
        items,
        created_at,
    }
}

#[test]
fn insert_event_round_trips_meta() {
    let store = mem_store();
    let mut event = ev("tool_output", "saida de ferramenta", "");
    event.meta = Some(r#"{"tool":"Read","file_path":"/tmp/a.rs"}"#.to_string());
    let id = store.insert_event(&event).unwrap();
    let meta: Option<String> = store
        .conn
        .query_row("SELECT meta FROM events WHERE id = ?1", [id], |r| r.get(0))
        .unwrap();
    assert_eq!(
        meta.as_deref(),
        Some(r#"{"tool":"Read","file_path":"/tmp/a.rs"}"#)
    );
}

#[test]
fn stats_scoped_filters_by_project_and_since_and_reports_min_created_at() {
    let store = mem_store();
    let mut old = ev("prompt", "primeiro evento aqui", "");
    old.created_at = 1_000;
    store.insert_event(&old).unwrap();
    let mut recent = ev("prompt", "segundo evento aqui", "");
    recent.created_at = 5_000;
    store.insert_event(&recent).unwrap();
    let mut other = ev("prompt", "outro projeto", "");
    other.project = "/tmp/outro".into();
    other.session_id = "s2".into();
    other.created_at = 9_000;
    store.insert_event(&other).unwrap();

    // Global, sem filtros: tudo, MIN = evento mais antigo.
    let (events, sessions, _tokens, first) = store.stats_scoped(None, None).unwrap();
    assert_eq!((events, sessions, first), (3, 2, Some(1_000)));

    // Escopo por projeto.
    let (events, _, _, first) = store.stats_scoped(Some("/tmp/proj"), None).unwrap();
    assert_eq!((events, first), (2, Some(1_000)));

    // since exclui o evento antigo (e o MIN acompanha o corte).
    let (events, _, _, first) = store.stats_scoped(Some("/tmp/proj"), Some(2_000)).unwrap();
    assert_eq!((events, first), (1, Some(5_000)));

    // Escopo sem nenhum evento: zeros e MIN ausente.
    let (events, sessions, tokens, first) = store.stats_scoped(Some("/nada"), None).unwrap();
    assert_eq!((events, sessions, tokens, first), (0, 0, 0, None));
}

#[test]
fn gain_summary_groups_by_kind_within_scope() {
    let store = mem_store();
    store
        .insert_gain(&gain("inject", "/p", 120, 3, 1_000))
        .unwrap();
    store
        .insert_gain(&gain("inject", "/p", 80, 2, 2_000))
        .unwrap();
    store
        .insert_gain(&gain("evict", "/p", 500, 4, 3_000))
        .unwrap();
    store
        .insert_gain(&gain("clear", "/q", 900, 7, 4_000))
        .unwrap();

    let rows = store.gain_summary(Some("/p"), None).unwrap();
    assert_eq!(
        rows,
        vec![
            ("evict".to_string(), 1, 4, 500),
            ("inject".to_string(), 2, 5, 200),
        ]
    );

    // since corta a primeira injeção; global inclui o clear de /q.
    let rows = store.gain_summary(None, Some(2_000)).unwrap();
    assert_eq!(
        rows,
        vec![
            ("clear".to_string(), 1, 7, 900),
            ("evict".to_string(), 1, 4, 500),
            ("inject".to_string(), 1, 2, 80),
        ]
    );
}

#[test]
fn gain_summary_on_a_db_without_the_table_is_empty_not_an_error() {
    // Um banco antigo aberto read-only pode não ter gain_ledger ainda
    // (a tabela só nasce num open read-write). Simula removendo-a.
    let store = mem_store();
    store.conn.execute("DROP TABLE gain_ledger", []).unwrap();
    assert_eq!(store.gain_summary(None, None).unwrap(), Vec::new());
}

#[test]
fn insert_and_search() {
    let store = mem_store();
    store
        .insert_event(&ev(
            "prompt",
            "fix the login authentication bug",
            "login auth bug",
        ))
        .unwrap();
    store
        .insert_event(&ev("tool_output", "cargo build finished ok", "cargo build"))
        .unwrap();

    let hits = store.search("authentication login", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, "prompt");
    assert!(hits[0].snippet.contains(">>"));
}

#[test]
fn search_scoped_by_project() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "unique marker alpha", ""))
        .unwrap();
    let hits = store.search("alpha", Some("/other"), 10).unwrap();
    assert!(hits.is_empty());
    let hits = store.search("alpha", Some("/tmp/proj"), 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn hostile_query_does_not_error() {
    let store = mem_store();
    store.insert_event(&ev("prompt", "content", "")).unwrap();
    for q in ["a\"b", "NOT AND OR", "(((", "col:val", "-x", "\"", ""] {
        store.search(q, None, 5).unwrap();
    }
}

#[test]
fn diacritics_match() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "configuração do serviço de memória", ""))
        .unwrap();
    let hits = store.search("configuracao memoria", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn events_delete_removes_fts_postings() {
    let store = mem_store();
    let id = store
        .insert_event(&ev("prompt", "conteudo exclusivo removido depois", ""))
        .unwrap();
    assert_eq!(
        store.search("exclusivo removido", None, 10).unwrap().len(),
        1
    );

    store
        .conn
        .execute("DELETE FROM events WHERE id = ?1", params![id])
        .unwrap();

    let hits = store.search("exclusivo removido", None, 10).unwrap();
    assert!(
        hits.is_empty(),
        "postings órfãos não devem ser retornados após DELETE"
    );

    // A corrupted external-content FTS index (orphaned postings pointing
    // at rows that no longer exist) fails this integrity check; a
    // healthy one is a silent no-op.
    store
        .conn
        .execute(
            "INSERT INTO events_fts(events_fts) VALUES ('integrity-check')",
            [],
        )
        .unwrap();
}

#[test]
fn selective_fts_query_does_not_prune_non_latin_diacritics() {
    let store = mem_store();
    // "ά" (alpha with tonos) is a diacritic unicode61's remove_diacritics
    // folds, but the old hand-rolled ASCII table (Latin-1 only) had no
    // entry for it — the query term stayed unfolded, its vocab lookup
    // (keyed on the folded form the indexer stored) always missed, and
    // the term was wrongly pruned as "absent from the corpus".
    for i in 0..200 {
        let mut e = ev("prompt", "deploy comum enchendo o corpus", "");
        e.session_id = format!("s{i}");
        store.insert_event(&e).unwrap();
    }
    let mut greek = ev("prompt", "σφάλμα ελληνικά raríssimo", "");
    greek.session_id = "grego".into();
    store.insert_event(&greek).unwrap();

    let hits = store.search_for_injection("ελληνικά", "atual", 5).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "termo grego com acento deve casar, não ser podado como ausente"
    );
    assert_eq!(hits[0].session_id, "grego");
}

#[test]
fn tokenize_probe_is_reused_across_calls() {
    // O probe FTS5 de tokenize_like_index agora é criado uma vez por
    // conexão (flag probe_ready) e reutilizado; este teste garante que a
    // reutilização não vaza estado entre chamadas — o DELETE continua
    // limpando o probe, então uma sequência de buscas acentuadas num
    // store "quente" retorna o mesmo que um store fresco.
    fn seeded() -> Store {
        let store = mem_store();
        let mut a = ev("prompt", "plano de ação para o deploy", "");
        a.session_id = "sa".into();
        store.insert_event(&a).unwrap();
        let mut b = ev("prompt", "máquina de café da copa", "");
        b.session_id = "sb".into();
        store.insert_event(&b).unwrap();
        store
    }
    fn run(store: &Store, q: &str) -> Vec<String> {
        store
            .search_for_injection(q, "atual", 5)
            .unwrap()
            .into_iter()
            .map(|h| h.session_id)
            .collect()
    }

    let warm = seeded();
    let first = run(&warm, "ação");
    let second = run(&warm, "café");

    let fresh = seeded();
    assert_eq!(first, run(&fresh, "ação"));
    let fresh = seeded();
    assert_eq!(second, run(&fresh, "café"));
    assert_eq!(first, vec!["sa".to_string()]);
    assert_eq!(second, vec!["sb".to_string()]);
}

#[test]
fn injection_prunes_corpus_common_terms() {
    let store = mem_store();
    for i in 0..200 {
        let mut e = ev("prompt", "deploy comum enchendo o corpus", "");
        e.session_id = format!("s{i}");
        store.insert_event(&e).unwrap();
    }
    let mut rare = ev("prompt", "deploy do serviço raríssimo", "");
    rare.session_id = "outra".into();
    store.insert_event(&rare).unwrap();

    // "deploy" está em >5% do corpus → podado; "rarissimo" fica.
    let hits = store
        .search_for_injection("deploy rarissimo", "atual", 5)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "outra");

    // Só termos comuns → query vazia → silêncio, não ruído.
    let hits = store
        .search_for_injection("deploy comum corpus", "atual", 5)
        .unwrap();
    assert!(hits.is_empty());
}

#[test]
fn injection_excludes_own_session() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "segredo exclusivo", ""))
        .unwrap();
    let hits = store
        .search_for_injection("segredo exclusivo", "s1", 5)
        .unwrap();
    assert!(hits.is_empty(), "própria sessão não deve ser reinjetada");
}

struct StubEmbedder;
impl Embedder for StubEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        if text == "animal" {
            vec![1.0, 0.0]
        } else {
            vec![0.0, 1.0]
        }
    }
    fn dim(&self) -> usize {
        2
    }
    fn id(&self) -> &str {
        "stub-2"
    }
}

#[test]
fn hybrid_rerank_flips_order_when_cosine_disagrees_with_bm25() {
    let store = mem_store();
    // "filler" is the sharpest match for "animal" (single word, no
    // dilution); it stretches the min-max bm25 range so "dog" (term
    // repeated, clearly better bm25 than "cat") and "cat" (single
    // occurrence, worst bm25) leave a gap smaller than the 0.4 weight
    // cosine can contribute.
    let filler_id = store.insert_event(&ev("prompt", "animal", "")).unwrap();
    let dog_id = store
        .insert_event(&ev("prompt", "animal animal foo bar baz qux quux", ""))
        .unwrap();
    let cat_id = store
        .insert_event(&ev("prompt", "animal foo bar baz qux quux", ""))
        .unwrap();

    let plain = store
        .search_hybrid("animal", None, 10, &StubEmbedder)
        .unwrap();
    assert_eq!(plain.len(), 3);
    assert_eq!(
        plain[0].id, filler_id,
        "documento mais focado no termo vence sem embeddings"
    );
    assert_eq!(
        plain[1].id, dog_id,
        "bm25 sozinho deve favorecer o documento com maior frequência do termo"
    );
    assert_eq!(plain[2].id, cat_id);

    // Embed only the cat event with a vector aligned to the query;
    // "dog" stays unembedded (falls back to 0.6 * normalized bm25).
    store
        .upsert_embedding(cat_id, "stub-2", &[1.0, 0.0])
        .unwrap();

    let hits = store
        .search_hybrid("animal", None, 10, &StubEmbedder)
        .unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(
        hits[0].id, filler_id,
        "vantagem de bm25 de \"filler\" ainda é grande demais para o cosine superar"
    );
    assert_eq!(
        hits[1].id, cat_id,
        "cosine deve superar a pequena desvantagem de bm25 e passar à frente de dog"
    );
    assert_eq!(hits[2].id, dog_id);
}

/// Mesmo `embed` do [`StubEmbedder`], mas declarando peso zero — o caso
/// de produção ([`crate::HashEmbedder`]) em miniatura, com vetores
/// controláveis para provar que o atalho de peso zero ignora o cosine.
struct ZeroWeightStub;
impl Embedder for ZeroWeightStub {
    fn embed(&self, text: &str) -> Vec<f32> {
        StubEmbedder.embed(text)
    }
    fn dim(&self) -> usize {
        2
    }
    fn id(&self) -> &str {
        "stub-2"
    }
    fn rerank_weight(&self) -> f64 {
        0.0
    }
}

#[test]
fn zero_weight_hybrid_keeps_pure_bm25_ordering() {
    // Mesmo corpus de hybrid_rerank_flips_order_when_cosine_disagrees_
    // with_bm25 (que prova que um embedder com peso > 0 ainda reordena):
    // aqui, com peso 0, o embedding favorável de "cat" NÃO pode passar à
    // frente — a ordem tem que ser exatamente a do bm25, igual ao caminho
    // completo com w=0 de antes do atalho.
    let store = mem_store();
    let filler_id = store.insert_event(&ev("prompt", "animal", "")).unwrap();
    let dog_id = store
        .insert_event(&ev("prompt", "animal animal foo bar baz qux quux", ""))
        .unwrap();
    let cat_id = store
        .insert_event(&ev("prompt", "animal foo bar baz qux quux", ""))
        .unwrap();
    store
        .upsert_embedding(cat_id, "stub-2", &[1.0, 0.0])
        .unwrap();

    let hits = store
        .search_hybrid("animal", None, 10, &ZeroWeightStub)
        .unwrap();
    let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
    assert_eq!(
        ids,
        vec![filler_id, dog_id, cat_id],
        "com peso 0 a ordem é bm25 puro — o cosine não pode influenciar"
    );
}

#[test]
fn open_bounded_creates_and_writes_like_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_bounded(&dir.path().join("ng.db")).unwrap();
    store.insert_event(&ev("prompt", "oi", "")).unwrap();
    let (events, _, _) = store.stats().unwrap();
    assert_eq!(events, 1);
}

#[test]
fn open_bounded_fails_fast_on_unusable_path() {
    // O path é um diretório, então cada tentativa de open falha na hora;
    // o variante bounded devolve erro em milissegundos, não nos ~3s do
    // retry de cold start do `Store::open` comum.
    let dir = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    assert!(Store::open_bounded(dir.path()).is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "open_bounded deveria falhar rápido, levou {:?}",
        started.elapsed()
    );
}

#[test]
fn open_rw_no_init_writes_on_initialized_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ng.db");
    // Schema garantido por um open completo (como o ngd faz no boot).
    drop(Store::open(&path).unwrap());

    let store = Store::open_rw_no_init(&path).unwrap();
    store
        .insert_event(&ev("prompt", "escrito sem re-DDL", ""))
        .unwrap();
    let (events, _, _) = store.stats().unwrap();
    assert_eq!(events, 1);
}

#[test]
fn open_rw_no_init_refuses_missing_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inexistente.db");
    assert!(Store::open_rw_no_init(&path).is_err());
    assert!(
        !path.exists(),
        "recusar não pode deixar para trás um banco vazio sem schema"
    );
}

#[test]
fn ann_recall_surfaces_semantic_match_without_fts_overlap() {
    // O caso semantic-gap puro: a query não compartilha NENHUM termo com o
    // documento-ouro, então o FTS devolve zero candidatos e nenhum rerank
    // poderia salvá-lo — só o recall ANN sobre os embeddings alcança o doc.
    let store = mem_store();
    let gold_id = store
        .insert_event(&ev("prompt", "config exclusiva do executor", ""))
        .unwrap();
    // Embedding alinhado ao vetor que StubEmbedder produz para "animal".
    store
        .upsert_embedding(gold_id, "stub-2", &[1.0, 0.0])
        .unwrap();

    let hits = store
        .search_hybrid("animal", None, 10, &StubEmbedder)
        .unwrap();
    assert_eq!(hits.len(), 1, "recall ANN deve achar o doc sem overlap FTS");
    assert_eq!(hits[0].id, gold_id);
    assert!(
        hits[0].snippet.contains("config exclusiva"),
        "hit ANN-only deve carregar um snippet de prefixo do conteúdo"
    );
    // bm25 floor 0 para ANN-only: score = w * cosine = 0.4 * 1.0.
    assert!((hits[0].rank - 0.4).abs() < 1e-9);
}

#[test]
fn ann_recall_skips_mismatched_dimension_embeddings() {
    // Embedding gravado com dimensão diferente da do embedder ativo (geração
    // antiga sob o mesmo id): nunca fazer cosine sobre lixo — o evento fica
    // fora do recall em vez de ganhar um score sem sentido.
    let store = mem_store();
    let id = store
        .insert_event(&ev("prompt", "config exclusiva do executor", ""))
        .unwrap();
    store
        .upsert_embedding(id, "stub-2", &[1.0, 1.0, 1.0])
        .unwrap();

    let hits = store
        .search_hybrid("animal", None, 10, &StubEmbedder)
        .unwrap();
    assert!(
        hits.is_empty(),
        "dim divergente deve ser pulada, não pontuada"
    );
}

#[test]
fn zero_weight_hybrid_has_no_ann_recall() {
    // Mesmo setup do teste de recall ANN, mas com peso 0 (o caminho default
    // do HashEmbedder): o atalho w == 0 tem que continuar byte-idêntico ao
    // comportamento pré-ANN — query sem match FTS retorna vazio, mesmo com
    // um embedding perfeitamente alinhado disponível.
    let store = mem_store();
    let id = store
        .insert_event(&ev("prompt", "config exclusiva do executor", ""))
        .unwrap();
    store.upsert_embedding(id, "stub-2", &[1.0, 0.0]).unwrap();

    let hits = store
        .search_hybrid("animal", None, 10, &ZeroWeightStub)
        .unwrap();
    assert!(
        hits.is_empty(),
        "com peso 0 não há recall ANN — caminho default inalterado"
    );
}

#[test]
fn ann_recall_merges_and_dedupes_with_fts() {
    // Evento coberto por FTS *e* ANN aparece uma única vez (com evidência
    // bm25); evento só-ANN entra atrás, com score puramente semântico.
    let store = mem_store();
    let both_id = store
        .insert_event(&ev("prompt", "animal raro avistado", ""))
        .unwrap();
    let ann_only_id = store
        .insert_event(&ev("prompt", "config exclusiva do executor", ""))
        .unwrap();
    store
        .upsert_embedding(both_id, "stub-2", &[1.0, 0.0])
        .unwrap();
    store
        .upsert_embedding(ann_only_id, "stub-2", &[1.0, 0.0])
        .unwrap();

    let hits = store
        .search_hybrid("animal", None, 10, &StubEmbedder)
        .unwrap();
    let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
    assert_eq!(
        ids,
        vec![both_id, ann_only_id],
        "dedupe por id: FTS+ANN uma vez (na frente, com bm25), só-ANN atrás"
    );
}

#[test]
fn hybrid_search_without_any_embeddings_does_not_break() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "deploy do serviço raro", ""))
        .unwrap();
    let hits = store
        .search_hybrid("deploy raro", None, 10, &StubEmbedder)
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn events_without_embedding_lists_backlog() {
    let store = mem_store();
    let id1 = store
        .insert_event(&ev("prompt", "conteudo um", ""))
        .unwrap();
    let id2 = store
        .insert_event(&ev("tool_output", "conteudo dois", ""))
        .unwrap();
    store.upsert_embedding(id1, "stub-2", &[0.0, 1.0]).unwrap();

    let backlog = store.events_without_embedding("stub-2", 10).unwrap();
    assert_eq!(backlog.len(), 1);
    assert_eq!(backlog[0].0, id2);
}

#[test]
fn encode_decode_vec_roundtrip() {
    let original = vec![0.5f32, -1.25, 3.0, 0.0];
    let bytes = encode_vec(&original);
    let decoded = decode_vec(&bytes);
    assert_eq!(original, decoded);
}

#[test]
fn stats_counts() {
    let store = mem_store();
    store.insert_event(&ev("prompt", "abcd efgh", "")).unwrap();
    let (events, sessions, tokens) = store.stats().unwrap();
    assert_eq!(events, 1);
    assert_eq!(sessions, 1);
    assert!(tokens >= 2);
}

#[test]
fn ingest_graph_extracts_every_kind() {
    let store = mem_store();
    store
        .ingest_graph(&ev("prompt", "build failed", "src/auth/login.rs error"))
        .unwrap();
    store
        .ingest_graph(&ev("prompt", "vamos usar rusqlite para tudo sempre", ""))
        .unwrap();

    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    let kinds: HashSet<&str> = entities.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains("file"),
        "esperava entidade file, achou {kinds:?}"
    );
    assert!(kinds.contains("error"));
    assert!(kinds.contains("decision"));
}

#[test]
fn ingest_graph_reinforces_weight_with_cap() {
    let store = mem_store();
    let event = ev("prompt", "build ok", "src/auth/login.rs auth");
    for _ in 0..150 {
        store.ingest_graph(&event).unwrap();
    }
    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    let file = entities
        .iter()
        .find(|e| e.name == "src/auth/login.rs")
        .unwrap();
    assert_eq!(
        file.weight, MAX_GRAPH_WEIGHT,
        "150 reaparições devem estourar o cap de 10.0"
    );
}

#[test]
fn decision_entity_starts_with_higher_initial_weight() {
    let store = mem_store();
    store
        .ingest_graph(&ev("prompt", "vamos usar rusqlite para tudo sempre", ""))
        .unwrap();
    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    let decision = entities.iter().find(|e| e.kind == "decision").unwrap();
    assert_eq!(decision.weight, DECISION_INITIAL_WEIGHT);
}

#[test]
fn ingest_graph_connects_cooccurring_entities() {
    let store = mem_store();
    store
        .ingest_graph(&ev("prompt", "build ok", "src/auth/login.rs auth login"))
        .unwrap();

    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    let ids: HashSet<i64> = entities.iter().map(|e| e.id).collect();
    let adjacency = store.load_adjacency(&ids).unwrap();
    let file = entities
        .iter()
        .find(|e| e.name == "src/auth/login.rs")
        .unwrap();
    // "auth" and "login" both cooccur with the file entity.
    assert_eq!(adjacency.get(&file.id).unwrap().len(), 2);
}

#[test]
fn neighbors_reaches_second_degree_with_decreasing_score() {
    let store = mem_store();
    // a—b from one event, b—c from another: "c" is only reachable
    // from "a" through "b".
    store.ingest_graph(&ev("prompt", "noop", "a b")).unwrap();
    store.ingest_graph(&ev("prompt", "noop", "b c")).unwrap();

    let hits = store.neighbors("a", Some("/tmp/proj"), 2, 10).unwrap();
    let names: Vec<&str> = hits.iter().map(|(e, _)| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["b", "c"],
        "b (1º grau) deve vir antes de c (2º grau)"
    );
    assert!(
        hits[0].1 > hits[1].1,
        "score deve decrescer com a distância: {:?}",
        hits
    );
}

#[test]
fn neighbors_unknown_entity_is_empty() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "noop", "a b")).unwrap();
    assert!(store
        .neighbors("does-not-exist", Some("/tmp/proj"), 2, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn bump_entity_clamps_to_bounds() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "noop", "topic")).unwrap();
    assert_eq!(store.bump_entity("topic", 50.0).unwrap(), 1);
    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    assert_eq!(
        entities.iter().find(|e| e.name == "topic").unwrap().weight,
        MAX_GRAPH_WEIGHT
    );

    store.bump_entity("topic", -50.0).unwrap();
    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    assert_eq!(
        entities.iter().find(|e| e.name == "topic").unwrap().weight,
        0.0
    );
}

#[test]
fn export_graph_md_has_sections_ordered_by_weight() {
    let store = mem_store();
    store
        .ingest_graph(&ev("prompt", "x", "src/a.rs src/b.rs"))
        .unwrap();
    // Reinforce b above a so ordering inside the "Arquivos" section is
    // meaningfully tested, not just insertion order.
    store.ingest_graph(&ev("prompt", "x", "src/b.rs")).unwrap();
    store
        .ingest_graph(&ev("prompt", "vamos usar sqlite sempre", ""))
        .unwrap();

    let md = store.export_graph_md(Some("/tmp/proj")).unwrap();
    assert!(md.contains("## Arquivos"));
    assert!(md.contains("## Decisões"));
    let b_pos = md.find("src/b.rs").unwrap();
    let a_pos = md.find("src/a.rs").unwrap();
    assert!(
        b_pos < a_pos,
        "src/b.rs tem peso maior e deve vir antes de src/a.rs"
    );
}

#[test]
fn export_graph_md_empty_store_has_no_sections() {
    let store = mem_store();
    let md = store.export_graph_md(None).unwrap();
    assert!(!md.contains("##"));
}

#[test]
fn graph_snapshot_without_focus_returns_top_weighted_with_edges() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "x", "a b")).unwrap();
    store.ingest_graph(&ev("prompt", "x", "a")).unwrap(); // reinforce "a" above "b"

    let (nodes, edges) = store
        .graph_snapshot(Some("/tmp/proj"), None, 2, 60)
        .unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].name, "a", "peso maior deve vir primeiro");
    assert_eq!(edges.len(), 1);
}

#[test]
fn graph_snapshot_respects_limit() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "x", "a b c d e")).unwrap();
    let (nodes, _) = store.graph_snapshot(Some("/tmp/proj"), None, 2, 3).unwrap();
    assert_eq!(nodes.len(), 3);
}

#[test]
fn graph_snapshot_with_focus_includes_focal_node_and_neighbors() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "x", "a b")).unwrap();
    store.ingest_graph(&ev("prompt", "x", "b c")).unwrap();

    let (nodes, _) = store
        .graph_snapshot(Some("/tmp/proj"), Some("a"), 2, 60)
        .unwrap();
    let names: HashSet<&str> = nodes.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains("a"), "nó focal deve estar presente");
    assert!(names.contains("b"));
    assert!(names.contains("c"), "vizinho de 2º grau também deve entrar");
}

#[test]
fn graph_snapshot_unknown_focus_is_empty() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "x", "a b")).unwrap();
    let (nodes, edges) = store
        .graph_snapshot(Some("/tmp/proj"), Some("nope"), 2, 60)
        .unwrap();
    assert!(nodes.is_empty());
    assert!(edges.is_empty());
}

#[test]
fn graph_ingest_pending_is_idempotent() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "build ok", "src/auth/login.rs auth"))
        .unwrap();
    store
        .insert_event(&ev("prompt", "vamos usar rusqlite sempre", ""))
        .unwrap();

    let first = store.graph_ingest_pending(100).unwrap();
    assert_eq!(first, 2);

    let second = store.graph_ingest_pending(100).unwrap();
    assert_eq!(
        second, 0,
        "sem eventos novos, a segunda passada não deve reprocessar nada"
    );

    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    let file = entities
        .iter()
        .find(|e| e.name == "src/auth/login.rs")
        .unwrap();
    assert_eq!(
        file.weight, DEFAULT_INITIAL_WEIGHT,
        "reingestão indevida inflaria o peso além do esperado"
    );

    // A third event arrives; only it should be picked up.
    store
        .insert_event(&ev("prompt", "build ok", "src/auth/login.rs auth"))
        .unwrap();
    let third = store.graph_ingest_pending(100).unwrap();
    assert_eq!(third, 1);
    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    let file = entities
        .iter()
        .find(|e| e.name == "src/auth/login.rs")
        .unwrap();
    assert!(
        file.weight > DEFAULT_INITIAL_WEIGHT,
        "a reaparição real deve reforçar o peso"
    );
}

#[test]
fn graph_ingest_pending_rolls_back_batch_and_cursor_together_on_error() {
    let store = mem_store();
    // First event has a single tag → only an entity upsert, no relation.
    store.insert_event(&ev("prompt", "x", "solo")).unwrap();
    // Second event has two tags → needs a relation upsert too.
    store.insert_event(&ev("prompt", "x", "b c")).unwrap();

    // Break the relations table so the second event's relation upsert
    // fails, forcing the whole batch — including the first event's
    // already-applied entity insert — and the cursor advance to roll
    // back together.
    store.conn.execute("DROP TABLE relations", []).unwrap();

    assert!(
        store.graph_ingest_pending(100).is_err(),
        "lote deve falhar quando a tabela relations não existe"
    );

    let cursor: i64 = store
        .conn
        .query_row(
            "SELECT last_event FROM graph_cursor WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor, 0,
        "cursor não deve avançar quando o lote falha no meio"
    );

    let entity_count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        entity_count, 0,
        "entidades do primeiro evento do lote também devem ser revertidas (mesma transação)"
    );
}

#[test]
fn graph_ingest_pending_normal_batch_is_atomic() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "build ok", "src/a.rs src/b.rs"))
        .unwrap();
    store
        .insert_event(&ev("prompt", "vamos usar rusqlite sempre", ""))
        .unwrap();

    let count = store.graph_ingest_pending(100).unwrap();
    assert_eq!(count, 2);

    let cursor: i64 = store
        .conn
        .query_row(
            "SELECT last_event FROM graph_cursor WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor, 2,
        "cursor deve avançar até o último evento do lote bem-sucedido"
    );

    let entities = store.load_scoped_entities(Some("/tmp/proj")).unwrap();
    assert!(entities.iter().any(|e| e.name == "src/a.rs"));
    assert!(entities.iter().any(|e| e.name == "src/b.rs"));
    assert!(entities.iter().any(|e| e.kind == "decision"));
}

#[test]
fn graph_ingest_reads_meta_for_typed_file_entities() {
    let store = mem_store();
    let mut e = ev("tool_output", "[Read] input: {}\noutput: ok", "");
    e.meta = Some(r#"{"tool":"Read","file_path":"/proj/src/a.rs"}"#.into());
    store.insert_event(&e).unwrap();
    store.graph_ingest_pending(10).unwrap();
    let (nodes, _) = store
        .graph_snapshot(Some("/tmp/proj"), None, 0, 50)
        .unwrap();
    assert!(nodes
        .iter()
        .any(|n| n.name == "/proj/src/a.rs" && n.kind == "file"));
}

#[test]
fn version_bump_wipes_derived_graph_but_never_events() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ng.db");
    {
        let store = Store::open(&db).unwrap();
        let e = ev(
            "prompt",
            "vamos usar rusqlite no projeto",
            "rusqlite projeto banco",
        );
        store.insert_event(&e).unwrap();
        store.graph_ingest_pending(10).unwrap();
        let (nodes, _) = store.graph_snapshot(None, None, 0, 50).unwrap();
        assert!(!nodes.is_empty());
        // Simula banco criado por regras antigas:
        store
            .conn
            .execute(
                "UPDATE graph_meta SET value = '1' WHERE key = 'rules_version'",
                [],
            )
            .unwrap();
    }
    // Reabrir dispara a migração de versão:
    let store = Store::open(&db).unwrap();
    let (nodes, edges) = store.graph_snapshot(None, None, 0, 50).unwrap();
    assert!(
        nodes.is_empty() && edges.is_empty(),
        "grafo derivado deveria ter sido zerado"
    );
    let events: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events, 1, "events é intocável");
    // Cursor zerado → re-ingestão reconstrói:
    assert!(store.graph_ingest_pending(10).unwrap() > 0);
    let (nodes, _) = store.graph_snapshot(None, None, 0, 50).unwrap();
    assert!(!nodes.is_empty());
}

#[test]
fn graph_rebuild_stamps_current_rules_version_so_reopen_does_not_rewipe() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ng.db");
    {
        let store = Store::open(&db).unwrap();
        store
            .insert_event(&ev("prompt", "vamos usar rusqlite no projeto", "rusqlite"))
            .unwrap();
        // Simula upgrade sem restart do daemon: estampa antiga em graph_meta.
        store
            .conn
            .execute(
                "UPDATE graph_meta SET value = '1' WHERE key = 'rules_version'",
                [],
            )
            .unwrap();
    }
    {
        // Caminho real do `ng wisdom --rebuild`: RW sem re-init.
        let store = Store::open_rw_no_init(&db).unwrap();
        assert_eq!(store.graph_rebuild().unwrap(), 1);
        let stamped: String = store
            .conn
            .query_row(
                "SELECT value FROM graph_meta WHERE key = 'rules_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stamped, super::graph::GRAPH_RULES_VERSION.to_string());
    }
    // Boot posterior (init + ensure_rules_version) NÃO deve re-wipar.
    let store = Store::open(&db).unwrap();
    let (nodes, _) = store.graph_snapshot(None, None, 0, 50).unwrap();
    assert!(
        !nodes.is_empty(),
        "reabertura re-wipou o grafo reconstruído pelo rebuild"
    );
}

#[test]
fn graph_rebuild_creates_graph_meta_when_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ng.db");
    {
        let store = Store::open(&db).unwrap();
        store
            .insert_event(&ev("prompt", "vamos usar rusqlite no projeto", "rusqlite"))
            .unwrap();
        // Simula banco criado por binário antigo, sem a tabela graph_meta —
        // o fluxo `open_rw_no_init` (CLI) nunca roda o init que a criaria.
        store.conn.execute("DROP TABLE graph_meta", []).unwrap();
    }
    {
        let store = Store::open_rw_no_init(&db).unwrap();
        assert_eq!(store.graph_rebuild().unwrap(), 1);
        let stamped: String = store
            .conn
            .query_row(
                "SELECT value FROM graph_meta WHERE key = 'rules_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stamped, super::graph::GRAPH_RULES_VERSION.to_string());
    }
    let store = Store::open(&db).unwrap();
    let (nodes, _) = store.graph_snapshot(None, None, 0, 50).unwrap();
    assert!(
        !nodes.is_empty(),
        "reabertura re-wipou o grafo reconstruído pelo rebuild"
    );
}

#[test]
fn graph_rebuild_reingests_everything_synchronously() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("ng.db")).unwrap();
    for i in 0..5 {
        let e = ev(
            "prompt",
            &format!("prompt numero {i} sobre rusqlite"),
            "rusqlite",
        );
        store.insert_event(&e).unwrap();
    }
    store.graph_ingest_pending(100).unwrap();
    let processed = store.graph_rebuild().unwrap();
    assert_eq!(processed, 5);
    let (nodes, _) = store.graph_snapshot(None, None, 0, 50).unwrap();
    assert!(nodes.iter().any(|n| n.name == "rusqlite"));
}

#[test]
fn upsert_embeddings_batch_stores_every_item() {
    let store = mem_store();
    let id1 = store.insert_event(&ev("prompt", "um", "")).unwrap();
    let id2 = store.insert_event(&ev("prompt", "dois", "")).unwrap();

    store
        .upsert_embeddings_batch("stub-2", &[(id1, vec![1.0, 0.0]), (id2, vec![0.0, 1.0])])
        .unwrap();

    let backlog = store.events_without_embedding("stub-2", 10).unwrap();
    assert!(
        backlog.is_empty(),
        "ambos os eventos devem ter embedding após o batch"
    );
}

#[test]
fn upsert_embeddings_batch_empty_is_noop() {
    let store = mem_store();
    store.upsert_embeddings_batch("stub-2", &[]).unwrap();
}

#[test]
fn load_adjacency_filters_in_sql_via_index() {
    let store = mem_store();
    store.ingest_graph(&ev("prompt", "x", "a b")).unwrap();

    // EXPLAIN QUERY PLAN's 4th column ("detail") describes how SQLite
    // resolves the query. "SEARCH ... USING INDEX" proves an index is
    // used instead of a full "SCAN" of the relations table — with
    // a IN(1) AND b IN(1) narrowing this hard, the planner actually
    // picks the PRIMARY KEY (a, b, kind) autoindex over
    // idx_relations_kind, which is a *better* outcome than the one
    // this test originally expected, not a worse one: either index
    // proves the IN(...) rewrite avoids materializing the whole table.
    let mut stmt = store
        .conn
        .prepare("EXPLAIN QUERY PLAN SELECT a, b, weight FROM relations WHERE kind = 'cooccurs' AND a IN (1) AND b IN (1)")
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    let plan_text = plan.join(" | ");
    assert!(
        plan_text.contains("SEARCH")
            && plan_text.contains("USING INDEX")
            && !plan_text.contains("SCAN TABLE"),
        "esperava um SEARCH indexado (não um SCAN completo), plano: {plan_text}"
    );
}

/// Regression test for a real startup race: `ngd` opens several
/// read-write connections (writer thread, enrichment worker) to a
/// brand-new database within milliseconds of each other, and the
/// `journal_mode = WAL` conversion on a not-yet-existing file was
/// observed failing with "database is locked" even with `busy_timeout`
/// set (reproduced manually ~2 out of 3 daemon startups before
/// `Store::open` grew its own retry loop). Every thread here must
/// eventually succeed.
#[test]
fn open_survives_concurrent_first_time_creation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ng.db");
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || Store::open(&path).is_ok())
        })
        .collect();
    for h in handles {
        assert!(
            h.join().unwrap(),
            "toda thread deve conseguir abrir a conexão eventualmente"
        );
    }
}

#[test]
fn hidden_memory_leaves_search_but_stays_in_list() {
    let store = mem_store();
    let id = store
        .insert_event(&ev("prompt", "unique marker zeta payload", "zeta"))
        .unwrap();

    // Visible: recallable and listed.
    assert_eq!(store.search("zeta", None, 10).unwrap().len(), 1);
    assert_eq!(store.list_memories(None, false, 10).unwrap().len(), 1);

    // Hide it: gone from every recall path, but the row still exists.
    assert!(store.hide_memory(id).unwrap());
    assert!(store.search("zeta", None, 10).unwrap().is_empty());
    assert!(store
        .search_for_injection("zeta", "other-session", 10)
        .unwrap()
        .is_empty());
    assert!(store
        .search_hybrid("zeta", None, 10, &crate::HashEmbedder)
        .unwrap()
        .is_empty());
    // Excluded from the default list, still returned with include_hidden.
    assert!(store.list_memories(None, false, 10).unwrap().is_empty());
    let all = store.list_memories(None, true, 10).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].hidden);

    // Content is untouched: nothing was deleted, only masked.
    assert_eq!(all[0].content, "unique marker zeta payload");

    // Restore it: recall comes back.
    assert!(store.unhide_memory(id).unwrap());
    assert_eq!(store.search("zeta", None, 10).unwrap().len(), 1);
    assert!(!store.list_memories(None, false, 10).unwrap()[0].hidden);
}

#[test]
fn add_manual_memory_is_searchable_and_listed() {
    let store = mem_store();
    let id = store
        .add_manual_memory("/tmp/proj", "remember the deploy runbook", "deploy runbook")
        .unwrap();
    assert!(id > 0);

    let hits = store.search("runbook", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].harness, "manual");

    let listed = store.list_memories(None, false, 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].manual);
    assert_eq!(listed[0].kind, "manual");
    assert!(!listed[0].hidden);
}

#[test]
fn annotate_sets_and_clears_note() {
    let store = mem_store();
    let id = store
        .insert_event(&ev("prompt", "content to annotate", ""))
        .unwrap();
    assert!(store.list_memories(None, false, 10).unwrap()[0]
        .note
        .is_none());

    assert!(store.annotate_memory(id, "  important context  ").unwrap());
    assert_eq!(
        store.list_memories(None, false, 10).unwrap()[0]
            .note
            .as_deref(),
        Some("important context")
    );

    // Empty/whitespace clears it.
    assert!(store.annotate_memory(id, "   ").unwrap());
    assert!(store.list_memories(None, false, 10).unwrap()[0]
        .note
        .is_none());
}

#[test]
fn edit_manual_memory_updates_content_and_fts() {
    let store = mem_store();
    let id = store
        .add_manual_memory("/tmp/proj", "remember the deploy runbook", "deploy")
        .unwrap();

    assert!(store
        .edit_memory_content(id, "zqxwv brand new runbook", "runbook v2")
        .unwrap());

    let listed = store.list_memories(None, false, 10).unwrap();
    assert_eq!(listed[0].content, "zqxwv brand new runbook");
    assert_eq!(listed[0].tags, "runbook v2");

    // The events_au trigger reindexed FTS: the new word hits, the old one
    // doesn't.
    assert_eq!(store.search("zqxwv", None, 10).unwrap().len(), 1);
    assert!(store.search("deploy", None, 10).unwrap().is_empty());
}

#[test]
fn edit_captured_memory_is_rejected() {
    let store = mem_store();
    let id = store
        .insert_event(&ev("prompt", "captured payload stays", ""))
        .unwrap();

    // Captured memories are read-only: nothing is mutated, ever.
    assert!(!store
        .edit_memory_content(id, "trying to overwrite captured", "x")
        .unwrap());
    assert_eq!(
        store.list_memories(None, false, 10).unwrap()[0].content,
        "captured payload stays"
    );
}

#[test]
fn edit_missing_memory_returns_false() {
    let store = mem_store();
    assert!(!store.edit_memory_content(999999, "nothing", "").unwrap());
}

#[test]
fn list_memories_respects_project_and_skips_markers() {
    let store = mem_store();
    store
        .insert_event(&ev("prompt", "project scoped memory", ""))
        .unwrap();
    store
        .insert_event(&ev("session_start", "should be skipped", ""))
        .unwrap();
    let mut other = ev("prompt", "other project memory", "");
    other.project = "/other".into();
    store.insert_event(&other).unwrap();

    // Global sees both prompts, never the session marker.
    let global = store.list_memories(None, false, 10).unwrap();
    assert_eq!(global.len(), 2);
    assert!(global.iter().all(|m| m.kind != "session_start"));

    // Scoped sees only its project.
    let scoped = store.list_memories(Some("/tmp/proj"), false, 10).unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].project, "/tmp/proj");
}

#[test]
fn init_is_idempotent_over_existing_db() {
    // Re-running init (as Store::open does on every daemon start) against a
    // database that already has the soft-state columns must not error on a
    // duplicate ALTER — ensure_column guards it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ng.db");
    {
        let store = Store::open(&path).unwrap();
        store
            .add_manual_memory("/p", "survives reopen", "")
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.list_memories(None, false, 10).unwrap().len(), 1);
}

#[test]
fn pending_transcript_imports_reads_meta_and_cursor() {
    let store = mem_store();
    let mut end = ev("session_end", "", "");
    end.session_id = "sessA".into();
    end.meta = Some(r#"{"transcript_path":"/tmp/sessA.jsonl"}"#.into());
    let id = store.insert_event(&end).unwrap();

    let scan = store.pending_transcript_imports(10).unwrap();
    assert_eq!(scan.imports.len(), 1);
    assert_eq!(scan.imports[0].transcript_path, "/tmp/sessA.jsonl");
    assert_eq!(scan.imports[0].event_id, id);
    assert_eq!(scan.max_scanned_id, id);

    store.assist_cursor_set(id).unwrap();
    assert!(store
        .pending_transcript_imports(10)
        .unwrap()
        .imports
        .is_empty());

    assert_eq!(store.transcript_imported_count("sessA").unwrap(), 0);
    store.set_transcript_imported_count("sessA", 7).unwrap();
    assert_eq!(store.transcript_imported_count("sessA").unwrap(), 7);
}
