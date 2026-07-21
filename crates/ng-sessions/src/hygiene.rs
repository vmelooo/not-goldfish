//! Lossless procedural hygiene: decide which items in a transcript are
//! safe to evict from the *live harness session file* to save context, and
//! rewrite them as recoverable stubs instead of deleting them outright.
//!
//! "Lossless" is the operative word — nothing here ever destroys data.
//! Eviction only replaces an item's `message.content` in the on-disk
//! transcript with a short stub pointing back at how to recover it: the
//! full content is already captured in the not-goldfish database (by the
//! capture hooks, independent of this crate) and a byte-for-byte backup of
//! the file is written before any rewrite (see [`crate::rewrite`]).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{SessionItem, Transcript};
use crate::rewrite::{rewrite_jsonl, split_lines};
use crate::Result;

/// Items within this many positions of the end of the transcript are "hot"
/// — the harness and the user are actively relying on them for context —
/// and are never eviction candidates, no matter their size or kind.
pub const HOT_ZONE: usize = 20;

/// Minimum estimated tokens for a tool output to be worth evicting. Small
/// tool results (a one-line `ok`, a short `ls`) aren't worth the stub
/// overhead.
const TOOL_LARGE_TOKENS: i64 = 100;

/// Minimum estimated tokens for an assistant text turn to be worth
/// evicting. Assistant prose is more likely to carry reasoning the user
/// still wants visible, so the bar is higher than for tool output.
const ASSISTANT_LARGE_TOKENS: i64 = 200;

/// How much further beyond the hot zone boundary an assistant text item
/// must sit before it's a candidate — assistant text is evicted more
/// conservatively than tool output, which is disposable by nature.
const ASSISTANT_EXTRA_OLD_AGE: usize = 20;

/// Per-item eviction relevance score. Lower `score` means a stronger
/// eviction candidate; non-candidates carry `f64::MAX` so a straightforward
/// ascending sort always ranks real candidates first.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemScore {
    pub index: usize,
    pub score: f64,
    pub evictable: bool,
    pub reason: &'static str,
    /// Carried alongside the score (not part of the spec'd shape, but
    /// `plan_eviction` needs the item's token weight to size a plan without
    /// re-walking the transcript, so it travels with the score instead).
    pub tokens_est: i64,
}

/// Score every item in `transcript` for eviction relevance. Never fails —
/// an item that can't be classified is simply marked not evictable.
pub fn score_items(transcript: &Transcript) -> Vec<ItemScore> {
    let total = transcript.items.len();
    let hot_zone_start = total.saturating_sub(HOT_ZONE);

    transcript
        .items
        .iter()
        .map(|item| score_one(item, total, hot_zone_start))
        .collect()
}

fn score_one(item: &SessionItem, total: usize, hot_zone_start: usize) -> ItemScore {
    let age = total.saturating_sub(item.index + 1); // 0 for the newest item
    let keep = |reason: &'static str| ItemScore {
        index: item.index,
        score: f64::MAX,
        evictable: false,
        reason,
        tokens_est: item.tokens_est,
    };

    // Hot zone wins over every other rule: recent context is never touched.
    if item.index >= hot_zone_start {
        return keep("within the last N items (hot zone)");
    }

    // Tool output is judged by *kind*, not role — Claude Code (and others)
    // deliver tool results on a "user" turn, but that's plumbing, not a
    // user decision, so it must not fall under the "never evict user" rule.
    if item.kind == "tool_result" {
        if item.tokens_est >= TOOL_LARGE_TOKENS {
            return ItemScore {
                index: item.index,
                score: -(item.tokens_est as f64 + age as f64),
                evictable: true,
                reason: "old, large tool output",
                tokens_est: item.tokens_est,
            };
        }
        return keep("tool output too small to bother evicting");
    }

    // Real user prompts and corrections are sacred: never auto-evicted.
    if item.role == "user" {
        return keep("user prompt — never evicted");
    }

    // Malformed / unrecognized items carry no reconstructable meaning
    // beyond their raw JSON preview; safe (and useful) to evict first.
    if item.kind == "other" || item.role == "other" {
        return ItemScore {
            index: item.index,
            score: -(item.tokens_est as f64) - 1_000_000.0,
            evictable: true,
            reason: "malformed or unrecognized item",
            tokens_est: item.tokens_est,
        };
    }

    // Assistant prose: only evict when both old and large.
    if item.role == "assistant" && item.kind == "text" {
        let old_enough = age >= HOT_ZONE + ASSISTANT_EXTRA_OLD_AGE;
        if old_enough && item.tokens_est >= ASSISTANT_LARGE_TOKENS {
            return ItemScore {
                index: item.index,
                score: -(item.tokens_est as f64 * 0.5 + age as f64 * 0.5),
                evictable: true,
                reason: "old, large assistant text",
                tokens_est: item.tokens_est,
            };
        }
        return keep("assistant text too recent or too small to evict");
    }

    // System messages, tool_use calls, mixed-kind items, etc. — kept: they
    // are either small plumbing or carry structural meaning we don't try
    // to second-guess here.
    keep("kind not covered by an eviction rule; kept conservatively")
}

/// A selected set of items to evict, chosen by ascending score (most
/// evictable first) until `target_tokens` estimated tokens are freed.
#[derive(Debug, Clone, PartialEq)]
pub struct EvictionPlan {
    pub drops: Vec<usize>,
    pub tokens_freed: i64,
}

/// Greedily select evictable items, most evictable first, until
/// `target_tokens` worth of estimated tokens would be freed (or evictable
/// candidates run out).
pub fn plan_eviction(scores: &[ItemScore], target_tokens: i64) -> EvictionPlan {
    let mut candidates: Vec<&ItemScore> = scores.iter().filter(|s| s.evictable).collect();
    candidates.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut drops = Vec::new();
    let mut tokens_freed = 0i64;
    for candidate in candidates {
        if tokens_freed >= target_tokens {
            break;
        }
        drops.push(candidate.index);
        tokens_freed += candidate.tokens_est;
    }
    drops.sort_unstable();

    EvictionPlan {
        drops,
        tokens_freed,
    }
}

/// Build the lossless stub text for an evicted item.
pub fn stub_for(item: &SessionItem, search_hint: &str) -> String {
    format!(
        "[ng-evicted: {} ~{}tok — recupere com: ng search {} | id interno preservado no banco]",
        item.kind, item.tokens_est, search_hint
    )
}

/// First few content-bearing words of a preview, used as the `ng search`
/// hint in a stub. Short/common tokens are dropped so the hint stays
/// selective rather than matching half the corpus.
fn derive_search_hint(preview: &str) -> String {
    let words: Vec<&str> = preview
        .split_whitespace()
        .filter(|w| w.chars().filter(|c| c.is_alphanumeric()).count() >= 3)
        .take(5)
        .collect();
    if words.is_empty() {
        "item".to_string()
    } else {
        words.join(" ")
    }
}

/// Result of applying an eviction plan to a Claude Code JSONL transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct EvictionApplyResult {
    /// Backup written by [`rewrite_jsonl`] before the swap.
    pub backup: PathBuf,
    /// Planned drops that could not be turned into a safe stub replacement
    /// (no `raw_line`, or the source line has no `message.content` to
    /// stub) and were left untouched rather than risking corruption.
    pub skipped: usize,
    /// Estimated tokens of the items *actually* stubbed (skipped drops
    /// excluded — `EvictionPlan::tokens_freed` still counts those).
    pub tokens_evicted_est: i64,
    /// Estimated tokens of the stubs left in their place (~4 bytes/token).
    /// `tokens_evicted_est - stub_tokens_est` is the net context saving a
    /// caller may record — the honest floor, net of what the stubs cost.
    pub stub_tokens_est: i64,
}

/// Apply `plan` to the Claude Code JSONL transcript at `path`. Items are
/// never dropped as lines — that would discard `uuid`/`parentUuid` chain
/// structure other items may reference — instead each evicted item's
/// `message.content` is replaced in place with a stub, and every other
/// field on that line (`uuid`, `parentUuid`, `type`, `cwd`, ...) is
/// preserved byte-for-byte from the original JSON.
pub fn apply_eviction_claude(
    path: &Path,
    transcript: &Transcript,
    plan: &EvictionPlan,
) -> Result<EvictionApplyResult> {
    let original = fs::read_to_string(path)?;
    // Mesma regra canônica de split do rewrite (finding 19b): quem numera
    // linhas e quem reescreve nunca podem discordar sobre qual linha é qual.
    let original_lines: Vec<&str> = split_lines(&original);

    // Index items by raw_line so we can look up each planned drop's target
    // line without re-scanning the transcript per drop.
    let items_by_index: HashMap<usize, &SessionItem> =
        transcript.items.iter().map(|i| (i.index, i)).collect();

    let mut skipped = 0usize;
    let mut tokens_evicted_est = 0i64;
    let mut stub_tokens_est = 0i64;
    let mut replacements: Vec<(usize, String)> = Vec::new();

    for &item_index in &plan.drops {
        let Some(item) = items_by_index.get(&item_index) else {
            skipped += 1;
            continue;
        };
        let Some(raw_line) = item.raw_line else {
            // Non-line-addressable format (shouldn't happen for Claude
            // Code, but never trust it blindly) — nothing safe to rewrite.
            skipped += 1;
            continue;
        };
        let Some(original_line) = original_lines.get(raw_line - 1) else {
            skipped += 1;
            continue;
        };

        let Ok(mut value) = serde_json::from_str::<Value>(original_line) else {
            skipped += 1;
            continue;
        };
        let Some(content_slot) = value
            .get_mut("message")
            .and_then(|m| m.as_object_mut())
            .map(|m| m.entry("content").or_insert(Value::Null))
        else {
            // No `message` object on this line: we don't know a safe shape
            // to stub into, so leave the original content in place rather
            // than guess.
            skipped += 1;
            continue;
        };

        let search_hint = derive_search_hint(&item.text_preview);
        let stub = stub_for(item, &search_hint);
        tokens_evicted_est += item.tokens_est;
        stub_tokens_est += (stub.len() / 4) as i64;
        *content_slot = Value::String(stub);

        replacements.push((raw_line, value.to_string()));
    }

    let backup = rewrite_jsonl(path, &[], &replacements)?;
    Ok(EvictionApplyResult {
        backup,
        skipped,
        tokens_evicted_est,
        stub_tokens_est,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude;
    use crate::model::SessionInfo;
    use std::io::Write;
    use std::time::SystemTime;

    /// Build a synthetic Claude Code transcript with:
    /// - index 0: a real user prompt (never evictable)
    /// - index 1: an assistant tool_use call (kept, no rule evicts it)
    /// - index 2: a large tool_result on a user turn (evictable: tool output)
    /// - index 3: a malformed/unrecognized item (evictable: "other")
    /// - index 4: a large, old-enough assistant text turn (evictable)
    /// - index 5: a second real user prompt (never evictable, even though old)
    /// - indices 6..25: small assistant text padding (kept: too small)
    /// - indices 25..45: the hot zone (kept: recent, regardless of shape)
    fn write_synthetic_transcript(dir: &Path) -> (PathBuf, Transcript) {
        let path = dir.join("session.jsonl");
        let big = "x".repeat(1000); // ~250 estimated tokens

        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"Please refactor the payment module for clarity."}},"uuid":"u0"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"Read","input":{{"path":"payment.rs"}}}}]}},"uuid":"a1","parentUuid":"u0"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}},"uuid":"u1","parentUuid":"a1"}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"unknown_future_event","weird":true}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":"{big}"}},"uuid":"a2","parentUuid":"u1"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"Actually, keep the retry logic as-is."}},"uuid":"u2","parentUuid":"a2"}}"#
        )
        .unwrap();
        for i in 6..25 {
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"role":"assistant","content":"ok {i}"}},"uuid":"pad{i}"}}"#
            )
            .unwrap();
        }
        for i in 25..45 {
            writeln!(
                f,
                r#"{{"type":"assistant","message":{{"role":"assistant","content":"recent {i}"}},"uuid":"hot{i}"}}"#
            )
            .unwrap();
        }

        let info = SessionInfo {
            id: "synthetic".to_string(),
            harness: claude::HARNESS.to_string(),
            path: path.clone(),
            project: None,
            modified_at: SystemTime::now(),
            items_hint: None,
        };
        let transcript = claude::parse(&info).unwrap();
        assert_eq!(transcript.skipped, 0);
        assert_eq!(transcript.items.len(), 45);
        (path, transcript)
    }

    #[test]
    fn scores_never_evict_user_or_hot_zone() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, transcript) = write_synthetic_transcript(tmp.path());
        let scores = score_items(&transcript);

        assert!(!scores[0].evictable, "real user prompt must be kept");
        assert!(!scores[5].evictable, "second user prompt must be kept");
        for score in &scores[25..45] {
            assert!(
                !score.evictable,
                "hot zone item {} must be kept",
                score.index
            );
        }
    }

    #[test]
    fn scores_flag_expected_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, transcript) = write_synthetic_transcript(tmp.path());
        let scores = score_items(&transcript);

        assert!(!scores[1].evictable, "tool_use call has no eviction rule");
        assert!(scores[2].evictable, "large tool_result should be evictable");
        assert!(scores[3].evictable, "malformed item should be evictable");
        assert!(
            scores[4].evictable,
            "old + large assistant text should be evictable"
        );
        for score in &scores[6..25] {
            assert!(
                !score.evictable,
                "small padding item {} should be kept",
                score.index
            );
        }
    }

    #[test]
    fn plan_eviction_matches_expected_candidates_and_token_sum() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, transcript) = write_synthetic_transcript(tmp.path());
        let scores = score_items(&transcript);
        let plan = plan_eviction(&scores, i64::MAX);

        assert_eq!(plan.drops, vec![2, 3, 4]);
        let expected_sum: i64 = scores
            .iter()
            .filter(|s| plan.drops.contains(&s.index))
            .map(|s| s.tokens_est)
            .sum();
        assert_eq!(plan.tokens_freed, expected_sum);
    }

    #[test]
    fn plan_eviction_respects_small_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, transcript) = write_synthetic_transcript(tmp.path());
        let scores = score_items(&transcript);
        // The malformed item (index 3) sorts first but only frees the few
        // tokens of its raw JSON fallback text, so a target set just above
        // that must keep pulling candidates in score order — index 2 next
        // — until the target is met, while still never reaching index 4
        // (the next-lowest-priority candidate).
        let malformed_tokens = scores.iter().find(|s| s.index == 3).unwrap().tokens_est;
        let plan = plan_eviction(&scores, malformed_tokens + 1);
        assert_eq!(plan.drops, vec![2, 3]);
        let index2_tokens = scores.iter().find(|s| s.index == 2).unwrap().tokens_est;
        assert_eq!(plan.tokens_freed, malformed_tokens + index2_tokens);
    }

    #[test]
    fn apply_eviction_claude_produces_valid_jsonl_with_preserved_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, transcript) = write_synthetic_transcript(tmp.path());
        let scores = score_items(&transcript);
        let plan = plan_eviction(&scores, i64::MAX);

        let result = apply_eviction_claude(&path, &transcript, &plan).unwrap();
        assert_eq!(result.skipped, 1, "index 3 has no `message` field to stub");
        assert!(result.backup.exists());

        let rewritten = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = rewritten.lines().collect();
        assert_eq!(lines.len(), 45, "line count must be unchanged");

        for line in &lines {
            serde_json::from_str::<Value>(line).expect("every rewritten line must stay valid JSON");
        }

        // Line 3 (1-based) = item index 2: tool_result, rewritten as a stub.
        let evicted_tool_result: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(evicted_tool_result["uuid"], "u1");
        assert_eq!(evicted_tool_result["parentUuid"], "a1");
        let stubbed_content = evicted_tool_result["message"]["content"].as_str().unwrap();
        assert!(stubbed_content.starts_with("[ng-evicted: tool_result"));
        assert!(stubbed_content.contains("ng search"));

        // Line 4 (1-based) = item index 3: malformed, has no `message` — must
        // be left byte-for-byte untouched since it was skipped.
        assert_eq!(lines[3], r#"{"type":"unknown_future_event","weird":true}"#);

        // Line 5 (1-based) = item index 4: assistant text, rewritten as a stub.
        let evicted_assistant: Value = serde_json::from_str(lines[4]).unwrap();
        assert_eq!(evicted_assistant["uuid"], "a2");
        assert_eq!(evicted_assistant["parentUuid"], "u1");
        let stubbed = evicted_assistant["message"]["content"].as_str().unwrap();
        assert!(stubbed.starts_with("[ng-evicted: text"));

        // Untouched lines, including both real user prompts, keep their
        // exact original content.
        let kept_user: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            kept_user["message"]["content"],
            "Please refactor the payment module for clarity."
        );
    }

    #[test]
    fn apply_eviction_claude_backup_matches_pre_rewrite_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, transcript) = write_synthetic_transcript(tmp.path());
        let before = fs::read(&path).unwrap();
        let scores = score_items(&transcript);
        let plan = plan_eviction(&scores, i64::MAX);

        let result = apply_eviction_claude(&path, &transcript, &plan).unwrap();
        let backup_bytes = fs::read(&result.backup).unwrap();
        assert_eq!(before, backup_bytes);
    }

    #[test]
    fn stub_mentions_kind_tokens_and_recovery_hint() {
        let item = SessionItem {
            index: 0,
            role: "user".to_string(),
            kind: "tool_result".to_string(),
            text_preview: "cargo build finished with warnings in auth module".to_string(),
            text_full: "cargo build finished with warnings in auth module".to_string(),
            tokens_est: 42,
            timestamp: None,
            raw_line: Some(1),
        };
        let hint = derive_search_hint(&item.text_preview);
        let stub = stub_for(&item, &hint);
        assert!(stub.contains("tool_result"));
        assert!(stub.contains("~42tok"));
        assert!(stub.contains("ng search"));
        assert!(stub.contains("cargo build finished"));
    }
}
