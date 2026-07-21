//! Sessões reais, não mocks: estes testes rodam contra transcripts
//! capturados de sessões de harness de verdade (ver `fixtures/real/`),
//! não contra arquivos mínimos escritos à mão. Os fixtures pequenos em
//! `fixtures/` seguem cobrindo casos-limite sintéticos (linha malformada,
//! formatos por harness); aqui a pergunta é outra: o parser e o rewrite
//! aguentam a realidade — queue-operations, attachments, sidechains,
//! tool_results gigantes e unicode — sem panic e sem perder linhas.

use std::path::PathBuf;
use std::time::SystemTime;

use ng_sessions::claude;
use ng_sessions::kimi;
use ng_sessions::model::SessionInfo;
use ng_sessions::rewrite::{rewrite_jsonl, split_lines};

fn real_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real")
}

fn info(harness: &str, id: &str, path: PathBuf, project: Option<&str>) -> SessionInfo {
    SessionInfo {
        id: id.to_string(),
        harness: harness.to_string(),
        path,
        project: project.map(|p| p.to_string()),
        modified_at: SystemTime::now(),
        items_hint: None,
    }
}

/// Transcript real de uma sessão `claude-code` (security review, jul/2026):
/// 37 linhas com queue-operations, attachments de memória, sidechains e
/// tool_results de dezenas de KB — o formato exatamente como o harness o
/// escreve, sem curadoria.
#[test]
fn claude_real_session_parses_end_to_end() {
    let path = real_dir().join("claude-security-review.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap();
    let non_empty_lines = raw.lines().filter(|l| !l.trim().is_empty()).count();

    let session = info(
        "claude-code",
        "f0e20691",
        path,
        Some("-home-vitor-work-leads-capture-leads-capturer-web"),
    );
    let transcript = claude::parse(&session).expect("sessão real deve parsear sem erro");

    assert!(
        transcript.items.len() >= 15,
        "uma sessão real rende itens de verdade (veio {})",
        transcript.items.len()
    );
    assert_eq!(
        transcript.items.len() + transcript.skipped,
        non_empty_lines,
        "toda linha não-vazia vira item ou skipped contado — nada some"
    );

    let mut last_raw = 0usize;
    for item in &transcript.items {
        assert!(
            matches!(
                item.role.as_str(),
                "user" | "assistant" | "tool" | "system" | "other"
            ),
            "role fora do vocabulário: {}",
            item.role
        );
        assert!(!item.kind.is_empty(), "kind sempre preenchido");
        let raw_line = item.raw_line.expect("item de transcript real tem raw_line");
        assert!(raw_line >= 1, "raw_line é 1-based");
        assert!(raw_line > last_raw, "raw_line estritamente crescente");
        last_raw = raw_line;
        assert!(item.tokens_est >= 0, "tokens_est nunca negativo");
        assert!(
            item.text_preview.chars().count() <= 220,
            "preview respeita o teto de tamanho"
        );
    }

    // Determinismo: parsear duas vezes dá o mesmo resultado.
    let again = claude::parse(&session).unwrap();
    assert_eq!(transcript.items.len(), again.items.len());
    assert_eq!(transcript.skipped, again.skipped);

    // Conteúdo real reconhecível: é uma sessão de security review.
    let corpus: String = transcript
        .items
        .iter()
        .map(|i| i.text_preview.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        corpus.contains("security") || corpus.contains("vulnerabilit"),
        "o conteúdo da sessão real deve ser reconhecível"
    );
}

/// O rewrite (backup + renumeração nunca alterada) sobre uma CÓPIA da
/// sessão real: drop remove exatamente a linha alvo e preserva as demais
/// byte-a-byte; o backup guarda o original integral.
#[test]
fn claude_real_session_rewrite_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("session.jsonl");
    std::fs::copy(real_dir().join("claude-security-review.jsonl"), &path).unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    let original_lines = split_lines(&before);
    let total = original_lines.len();

    let backup = rewrite_jsonl(&path, &[3], &[]).expect("rewrite em sessão real");
    assert!(backup.exists(), "backup sempre é escrito antes");
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        before,
        "backup é o original byte-a-byte"
    );

    let after = std::fs::read_to_string(&path).unwrap();
    let remaining = split_lines(&after);
    assert_eq!(
        remaining.len(),
        total - 1,
        "drop remove exatamente uma linha"
    );
    let expected: Vec<&str> = original_lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 2) // linha 3 (1-based)
        .map(|(_, l)| *l)
        .collect();
    assert_eq!(
        remaining, expected,
        "demais linhas preservadas byte-a-byte, na ordem"
    );

    // E a cópia reescrita continua parseável pelo parser real.
    let session = info("claude-code", "f0e20691-copy", path, Some("proj"));
    let transcript = claude::parse(&session).expect("transcript reescrito continua parseável");
    assert_eq!(transcript.items.len() + transcript.skipped, total - 1);
}

/// O discovery do kimi cobre os dois layouts reais: o atual do Kimi Code
/// CLI (`~/.kimi-code/sessions/<ws>/<id>/agents/<agent>/wire.jsonl`) e o
/// legado (`~/.kimi/sessions/<ws>/<id>/wire.jsonl`) — a estrutura de
/// diretórios é a real, só o conteúdo do wire é mínimo.
#[test]
fn kimi_discovers_current_and_legacy_layouts() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    let current = home.join(".kimi-code/sessions/wd_proj_ab12/sess-1/agents/main");
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(
        current.join("wire.jsonl"),
        "{\"role\":\"user\",\"content\":\"oi\"}\n",
    )
    .unwrap();

    let legacy = home.join(".kimi/sessions/proj/sess-0");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("wire.jsonl"),
        "{\"role\":\"user\",\"content\":\"oi\"}\n",
    )
    .unwrap();

    let found = kimi::discover(home);
    let mut ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["sess-0", "sess-1/main"], "ambos os layouts");
    assert!(found.iter().all(|s| s.harness == "kimi"));

    let session = found
        .iter()
        .find(|s| s.id == "sess-1/main")
        .expect("wire do layout atual descoberto");
    let transcript = kimi::parse(session).expect("wire atual parseia");
    assert_eq!(transcript.items.len(), 1);
}
