//! PreCompact hygiene: opt-in eviction of low-value transcript items right
//! before the harness compacts context, so the compact summarizes a
//! transcript that already had disposable bulk (old tool output, stale
//! filler) replaced by short, recoverable stubs — see
//! `ng_sessions::hygiene` for the actual scoring/eviction/stub logic.
//!
//! OPT-IN, HARD GATE: rewriting a *live* session's transcript file during a
//! harness-driven compact is sensitive territory — a bug here could
//! corrupt an in-progress session, or race the harness's own read of the
//! file. This only ever runs when the caller passes `force_enabled: true`.
//! `main` is the only caller, and it only passes `true` when the operator
//! has explicitly set `NG_AUTO_HYGIENE=1`. The default is off: with the
//! flag false, this function reads nothing, writes nothing, and returns
//! `None` before touching the payload at all.
//!
//! Every other failure mode — payload missing fields, transcript unreadable,
//! parse error, rewrite error — is swallowed into `None`. Hygiene must
//! never be the reason a compact, or the session around it, breaks.

use std::path::Path;

use ng_core::GainRecord;
use ng_sessions::hygiene::{apply_eviction_claude, plan_eviction, score_items};
use ng_sessions::{claude, SessionInfo};

/// Default token budget to try to free per PreCompact pass. Overridable via
/// `NG_HYGIENE_TARGET_TOKENS` so operators can tune how aggressive the pass
/// is without a rebuild.
const DEFAULT_TARGET_TOKENS: i64 = 20_000;

/// What a successful hygiene pass produced: the hook response JSON to print
/// on stdout, plus the gain-ledger record describing what the rewrite
/// netted. The record exists only *after* the atomic rename inside
/// `apply_eviction_claude` succeeded — the ledger never counts a rewrite
/// that failed. Delivering it is the caller's problem (and best-effort:
/// metric loss is acceptable, breaking the session is not).
pub struct PrecompactOutcome {
    pub response: String,
    pub gain: GainRecord,
}

/// Handle one `PreCompact` hook invocation.
///
/// `payload_json` is the raw JSON the harness wrote to stdin. `force_enabled`
/// must be the caller's read of `NG_AUTO_HYGIENE == "1"` — this function
/// does not read the environment for the gate itself, so tests can drive it
/// deterministically instead of racing on process-global env state.
///
/// Returns the [`PrecompactOutcome`] when at least one item was actually
/// turned into a stub; `None` in every other case (gate off, wrong event,
/// malformed payload, nothing evictable, or any I/O/parse error along the
/// way).
pub fn handle_precompact(payload_json: &str, force_enabled: bool) -> Option<PrecompactOutcome> {
    if !force_enabled {
        return None;
    }

    let payload: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    if payload.get("hook_event_name")?.as_str()? != "PreCompact" {
        return None;
    }
    let transcript_path = payload.get("transcript_path")?.as_str()?;
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let info = SessionInfo {
        id: session_id,
        harness: claude::HARNESS.to_string(),
        path: Path::new(transcript_path).to_path_buf(),
        project: None,
        modified_at: std::time::SystemTime::now(),
        items_hint: None,
    };

    let transcript = claude::parse(&info).ok()?;
    let scores = score_items(&transcript);
    let target_tokens = env_parse("NG_HYGIENE_TARGET_TOKENS", DEFAULT_TARGET_TOKENS);
    let plan = plan_eviction(&scores, target_tokens);
    if plan.drops.is_empty() {
        return None;
    }

    let result = apply_eviction_claude(Path::new(transcript_path), &transcript, &plan).ok()?;
    let stubbed = plan.drops.len().saturating_sub(result.skipped);
    if stubbed == 0 {
        // Every planned drop turned out unsafe to stub (no raw_line, no
        // `message` field); apply_eviction_claude still ran rewrite_jsonl
        // as a no-op backup+swap, but there is nothing worth telling the
        // harness about.
        return None;
    }

    let summary = format!(
        "not-goldfish: {stubbed} item(ns) da sessão viraram stub (~{}tok liberados). Backup em {}.",
        plan.tokens_freed,
        result.backup.display(),
    );

    let response = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreCompact",
            "additionalContext": summary,
        }
    })
    .to_string();

    // Economia líquida: tokens dos itens realmente stubados menos os tokens
    // dos próprios stubs — o piso conservador do plano 003 (contado 1x).
    let gain = GainRecord {
        kind: "evict".to_string(),
        session_id: info.id,
        project: payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tokens: (result.tokens_evicted_est - result.stub_tokens_est).max(0),
        items: stubbed as i64,
        created_at: now_epoch(),
    };

    Some(PrecompactOutcome { response, gain })
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_gate_is_a_pure_noop() {
        // No transcript_path at all — if the gate were checked after
        // payload parsing this would already fail for a different reason,
        // so this also proves the gate short-circuits before touching the
        // payload.
        assert!(handle_precompact("not even json", false).is_none());
    }

    #[test]
    fn wrong_event_name_is_ignored_even_when_enabled() {
        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "transcript_path": "/tmp/does-not-matter.jsonl",
            "session_id": "s1",
        })
        .to_string();
        assert!(handle_precompact(&payload, true).is_none());
    }

    #[test]
    fn missing_transcript_path_is_ignored() {
        let payload = serde_json::json!({
            "hook_event_name": "PreCompact",
            "session_id": "s1",
        })
        .to_string();
        assert!(handle_precompact(&payload, true).is_none());
    }

    /// Transcript sintético mínimo (mesma forma do usado em `ng clear`): um
    /// prompt, um tool_result grande e frio, e padding suficiente para
    /// empurrá-lo para fora da hot zone.
    fn write_min_transcript(dir: &Path) -> std::path::PathBuf {
        use std::io::Write;
        let path = dir.join("session.jsonl");
        let big = "x".repeat(6000);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"Refatore o módulo de pagamento."}},"uuid":"u0"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}},"uuid":"u1","parentUuid":"a1"}}"#
        )
        .unwrap();
        for i in 2..25 {
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"role":"assistant","content":"ok {i}"}},"uuid":"pad{i}"}}"#
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn successful_pass_reports_net_gain_after_the_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = write_min_transcript(tmp.path());
        let payload = serde_json::json!({
            "hook_event_name": "PreCompact",
            "transcript_path": transcript.to_string_lossy(),
            "session_id": "s1",
            "cwd": "/tmp/proj",
        })
        .to_string();

        let outcome = handle_precompact(&payload, true).expect("um item frio deveria virar stub");
        // O rewrite já aconteceu (o gain só existe depois do rename)...
        assert!(std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("[ng-evicted:"));
        // ...e o registro é o piso líquido: itens stubados, tokens dos itens
        // menos os tokens dos stubs, nunca negativo.
        assert_eq!(outcome.gain.kind, "evict");
        assert_eq!(outcome.gain.project, "/tmp/proj");
        assert_eq!(outcome.gain.session_id, "s1");
        assert_eq!(outcome.gain.items, 1);
        assert!(
            outcome.gain.tokens > 0 && outcome.gain.tokens < 1500,
            "economia líquida deve ser positiva e menor que o item bruto (~1500tok), foi {}",
            outcome.gain.tokens
        );
    }

    #[test]
    fn unreadable_transcript_is_ignored() {
        let payload = serde_json::json!({
            "hook_event_name": "PreCompact",
            "transcript_path": "/nonexistent/path/session.jsonl",
            "session_id": "s1",
        })
        .to_string();
        assert!(handle_precompact(&payload, true).is_none());
    }
}
