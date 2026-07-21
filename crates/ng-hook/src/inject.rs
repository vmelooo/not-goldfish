//! Proactive memory injection for UserPromptSubmit.
//!
//! After capturing the prompt we search past sessions for relevant
//! memories and emit them as `additionalContext` in the hook response.
//! Grounding rules (anti-hallucination, from the design):
//! - every injected memory carries provenance (event id, harness, date);
//! - only hits above a relevance threshold are injected — irrelevant
//!   context induces hallucination, so silence beats noise;
//! - a hard token budget caps the injection;
//! - the current session is never re-injected (it is already in context).

use ng_core::{timeutil, SearchHit, Store};

/// bm25 scores are negative (lower = better). Hits worse than this are
/// dropped even if they are the best available.
const DEFAULT_MAX_RANK: f64 = -1.0;
const DEFAULT_LIMIT: usize = 3;
const DEFAULT_TOKEN_BUDGET: usize = 600;

/// One built injection plus its declared cost — the caller reports these
/// numbers to the `gain_ledger` as *custo* (tokens que entraram no prompt),
/// nunca como economia (plano 003 A.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Injection {
    /// The `<not-goldfish-memory>` block to emit as additionalContext.
    pub context: String,
    /// Memories actually included (post-dedup, post-budget).
    pub items: i64,
    /// Estimated tokens of `context` — same ~4 bytes/token heuristic as the
    /// `NG_INJECT_BUDGET` cap it was built under.
    pub tokens_est: i64,
}

pub fn build_injection(store: &Store, prompt: &str, session_id: &str) -> Option<Injection> {
    if env_flag_disabled("NG_INJECT") {
        return None;
    }
    let limit = env_parse("NG_INJECT_LIMIT", DEFAULT_LIMIT);
    let max_rank = env_parse("NG_INJECT_MAX_RANK", DEFAULT_MAX_RANK);
    let budget_tokens = env_parse("NG_INJECT_BUDGET", DEFAULT_TOKEN_BUDGET);

    let hits = store
        .search_for_injection(prompt, session_id, limit * 3)
        .ok()?;
    // Near-duplicate memories (same prompt captured from parallel sessions,
    // retried commands) would fill every injection slot with one fact.
    let mut seen = std::collections::HashSet::new();
    let selected: Vec<&SearchHit> = hits
        .iter()
        .filter(|hit| hit.rank <= max_rank)
        .filter(|hit| seen.insert(normalize(&hit.snippet)))
        .take(limit)
        .collect();
    if selected.is_empty() {
        return None;
    }

    let mut out = String::from(
        "<not-goldfish-memory>\nMemórias de sessões anteriores relevantes ao pedido \
         (proveniência entre colchetes; use `ng search <termos>` para recuperar mais):\n",
    );
    let budget_bytes = budget_tokens * 4;
    let mut items = 0i64;
    for hit in selected {
        let line = format!(
            "- [#{} · {} · {}] {}\n",
            hit.id,
            hit.harness,
            timeutil::fmt_date(hit.created_at),
            hit.snippet.replace('\n', " "),
        );
        if out.len() + line.len() > budget_bytes {
            break;
        }
        out.push_str(&line);
        items += 1;
    }
    if items == 0 {
        // Every selected hit was over budget — an empty envelope injects
        // nothing useful and would still be recorded as served.
        return None;
    }
    out.push_str("</not-goldfish-memory>");
    let tokens_est = (out.len() / 4) as i64;
    Some(Injection {
        context: out,
        items,
        tokens_est,
    })
}

/// Render the Claude Code hook response envelope.
pub fn hook_response(context: String) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context,
        }
    })
    .to_string()
}

/// Collapse whitespace, lowercase, and drop trailing digits per token so
/// "autostart 1" and "autostart 3" dedup to the same key.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            token
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .to_lowercase()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn env_flag_disabled(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// [finding 01] Hot-path entry point for `UserPromptSubmit`: opens the
/// database **read-only** and returns `None` on any failure to open or
/// query it — a missing database, a schema that doesn't exist yet, or a
/// query against a not-yet-created `fts5vocab` table all degrade to
/// silence (no injection) rather than falling back to a write-capable
/// open. By the time a prompt reaches this call, the database is expected
/// to already exist and have its schema: either `ngd` created it, this
/// same hook invocation's capture path just did a `Store::open` (RW) of
/// its own a few lines above in `main.rs`, or `ng install` already ran one
/// `Store::open` (RW) up front specifically so the very first prompt has
/// something to read. Opening RW here instead would put the idempotent
/// schema-init DDL, its retry loop, and `busy_timeout` back on every
/// single prompt's hot path — exactly what this function exists to avoid.
pub fn build_injection_readonly(
    db_path: &std::path::Path,
    prompt: &str,
    session_id: &str,
) -> Option<Injection> {
    let store = Store::open_readonly(db_path).ok()?;
    build_injection(&store, prompt, session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ng_core::Event;

    fn seed_event(store: &Store, session_id: &str, project: &str, content: &str, created_at: i64) {
        store
            .insert_event(&Event {
                session_id: session_id.to_string(),
                project: project.to_string(),
                harness: "claude-code".to_string(),
                kind: "prompt".to_string(),
                content: content.to_string(),
                tags: String::new(),
                meta: None,
                created_at,
            })
            .unwrap();
    }

    /// bm25's IDF term needs an actual corpus to be discriminating — with
    /// only 1-2 documents total, even an exact-phrase match scores a rank
    /// far short of `DEFAULT_MAX_RANK` (empirically ~-0.00001 instead of
    /// something like -16), so `build_injection` would filter it out and
    /// these tests would wrongly look like a real feature. This mirrors
    /// what a real not-goldfish database looks like after even a single
    /// day of use — never a 1-document corpus.
    fn seed_filler_corpus(store: &Store) {
        for i in 0..30 {
            seed_event(
                store,
                &format!("filler-{i}"),
                "/proj",
                &format!("configurado cache redis para acelerar consultas do dashboard numero {i}"),
                1_784_368_800,
            );
        }
    }

    #[test]
    fn build_injection_readonly_returns_none_when_db_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("does-not-exist").join("ng.db");
        let result = build_injection_readonly(&db_path, "some prompt", "session-a");
        assert!(result.is_none());
        assert!(
            !db_path.exists(),
            "a read-only open must never create the database file"
        );
    }

    #[test]
    fn build_injection_readonly_finds_relevant_memory_in_a_precreated_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("ng.db");

        // Pre-create the database RW, exactly like the daemon or `ng
        // install` would — the read-only path under test never does this
        // itself.
        let store = Store::open(&db_path).unwrap();
        seed_filler_corpus(&store);
        seed_event(
            &store,
            "session-a",
            "/proj",
            "corrigido bug de autenticacao no login o token expirava antes do refresh disparar",
            1_784_368_800,
        );
        drop(store);

        let result = build_injection_readonly(
            &db_path,
            "o token esta expirando de novo antes do refresh, mesmo bug de autenticacao no login",
            "session-b",
        );
        assert!(
            result.is_some(),
            "a relevant memory in a pre-created db must be found read-only"
        );
        assert!(result.unwrap().context.contains("not-goldfish-memory"));
    }

    #[test]
    fn build_injection_reports_items_and_token_cost_consistent_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("ng.db");
        let store = Store::open(&db_path).unwrap();
        seed_filler_corpus(&store);
        seed_event(
            &store,
            "session-a",
            "/proj",
            "corrigido bug de autenticacao no login o token expirava antes do refresh disparar",
            1_784_368_800,
        );

        let injection = build_injection(
            &store,
            "o token esta expirando de novo antes do refresh, mesmo bug de autenticacao no login",
            "session-b",
        )
        .expect("relevant memory should be found");
        // items = provenance lines actually emitted; tokens = declared cost
        // of the whole block under the same ~4 bytes/token heuristic.
        assert_eq!(
            injection.items,
            injection.context.matches("\n- [#").count() as i64
        );
        assert!(injection.items >= 1);
        assert_eq!(injection.tokens_est, (injection.context.len() / 4) as i64);
    }

    #[test]
    fn build_injection_uses_shared_timeutil_date_format() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("ng.db");
        let store = Store::open(&db_path).unwrap();
        seed_filler_corpus(&store);
        seed_event(
            &store,
            "session-a",
            "/proj",
            "corrigido bug de autenticacao no login o token expirava antes do refresh disparar",
            1_784_368_800,
        );

        let injection = build_injection(
            &store,
            "o token esta expirando de novo antes do refresh, mesmo bug de autenticacao no login",
            "session-b",
        )
        .expect("relevant memory should be found");
        assert!(
            injection
                .context
                .contains(&timeutil::fmt_date(1_784_368_800)),
            "provenance date must use ng_core::timeutil::fmt_date"
        );
    }
}
