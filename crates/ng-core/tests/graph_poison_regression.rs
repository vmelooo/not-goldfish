//! Regressão ponta-a-ponta do saneamento do grafo de sabedoria.
//!
//! Os vetores de envenenamento diagnosticados na fase — SQL/JSON em
//! `tool_output`, saída do próprio ng re-capturada (`ng_meta`), `system`
//! do Codex, "use" dentro de pergunta lido como marcador de decisão — não
//! podem voltar a gerar nós. O caminho exercitado é o público real:
//! `insert_event` → `graph_ingest_pending` → `graph_snapshot`.

use ng_core::{Event, Store};
use tempfile::TempDir;

fn ev(kind: &str, content: &str, tags: &str) -> Event {
    Event {
        session_id: "s1".into(),
        project: "/proj".into(),
        harness: "claude-code".into(),
        kind: kind.into(),
        content: content.into(),
        tags: tags.into(),
        meta: None,
        created_at: 1_700_000_000,
    }
}

#[test]
fn poison_vectors_produce_no_nodes() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(&tmp.path().join("ng.db")).unwrap();

    // Saída de ferramenta com SQL/JSON — as tags lexicais (inclusive a chave
    // JSON "command") existem no evento (captura/busca intactas), mas o grafo
    // não pode consumi-las.
    store
        .insert_event(&ev(
            "tool_output",
            "[Bash] input: {\"command\":\"sqlite3 ng.db\"}\noutput: SELECT id FROM events WHERE id > ?1; INSERT INTO entities VALUES (1);",
            "select events entities insert sqlite3 ng.db command",
        ))
        .unwrap();
    // Saída do próprio ng re-capturada — o hook classifica como ng_meta.
    store
        .insert_event(&ev(
            "ng_meta",
            "[Bash] input: {\"command\":\"ng wisdom\"}\noutput: ● rusqlite peso 9.1\n[ng-evicted: abc]",
            "rusqlite peso ng-evicted",
        ))
        .unwrap();
    // System prompt do Codex (dispatch é por kind; o harness auto-documenta
    // a regressão que o spec nomeia).
    let mut sys = ev(
        "system",
        "You are a helpful coding agent…",
        "helpful coding agent",
    );
    sys.harness = "codex".into();
    store.insert_event(&sys).unwrap();
    // Falso-positivo de decisão: "use" numa pergunta não é marcador.
    store
        .insert_event(&ev("prompt", "how do I use this crate?", "crate"))
        .unwrap();

    assert_eq!(store.graph_ingest_pending(100).unwrap(), 4);
    let (nodes, _) = store.graph_snapshot(None, None, 0, 100).unwrap();

    assert!(
        !nodes.iter().any(|n| n.kind == "decision"),
        "decisão espúria a partir de 'use' em pergunta"
    );
    for poison in [
        "select",
        "insert",
        "entities",
        "sqlite3",
        "command",
        "helpful",
        "ng-evicted",
        "peso",
        "rusqlite",
    ] {
        assert!(
            !nodes.iter().any(|n| n.name == poison),
            "nó envenenado sobreviveu: {poison}"
        );
    }
    // O prompt legítimo ainda gera concept — prova que o ingest rodou e que
    // o teste não passa por vacuidade (grafo vazio passaria em tudo acima).
    assert!(nodes
        .iter()
        .any(|n| n.name == "crate" && n.kind == "concept"));
}

#[test]
fn dialogue_and_typed_entities_do_land() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(&tmp.path().join("ng.db")).unwrap();

    store
        .insert_event(&ev(
            "prompt",
            "decidimos migrar o layout do grafo para radial",
            "layout grafo radial",
        ))
        .unwrap();
    store
        .insert_event(&ev(
            "assistant",
            "o layout radial evita cruzamento de arestas",
            "layout radial arestas",
        ))
        .unwrap();
    let mut tool = ev(
        "tool_output",
        "[Bash] input: {}\noutput: error[E0502]: cannot borrow",
        "",
    );
    tool.meta = Some(r#"{"tool":"Bash","file_path":"/proj/src/ui.rs"}"#.into());
    store.insert_event(&tool).unwrap();

    assert_eq!(store.graph_ingest_pending(100).unwrap(), 3);
    let (nodes, edges) = store.graph_snapshot(None, None, 0, 100).unwrap();

    assert!(
        nodes.iter().any(|n| n.kind == "decision"),
        "'decidimos' com word-boundary deve continuar virando nó de decisão"
    );
    assert!(nodes
        .iter()
        .any(|n| n.name == "layout" && n.kind == "concept"));
    // "arestas" só existe nas tags do evento assistant — pina que o kind
    // assistant continua no dispatch do grafo.
    assert!(
        nodes
            .iter()
            .any(|n| n.name == "arestas" && n.kind == "concept"),
        "assistant do transcript deve entrar no grafo"
    );
    assert!(nodes
        .iter()
        .any(|n| n.name == "/proj/src/ui.rs" && n.kind == "file"));
    assert!(nodes.iter().any(|n| n.name == "E0502" && n.kind == "error"));
    // Aresta nomeada, não `!edges.is_empty()`: co-ocorrência qualquer (ex.
    // interna ao prompt) não pode satisfazer a asserção no lugar da real.
    let id_of = |name: &str| {
        nodes
            .iter()
            .find(|n| n.name == name)
            .map(|n| n.id)
            .expect(name)
    };
    let (layout, radial) = (id_of("layout"), id_of("radial"));
    assert!(
        edges
            .iter()
            .any(|&(a, b, _)| (a == layout && b == radial) || (a == radial && b == layout)),
        "aresta de co-ocorrência layout↔radial deve existir"
    );
}
