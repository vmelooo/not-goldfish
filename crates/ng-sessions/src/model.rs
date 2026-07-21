//! Session/transcript data model shared by all harness parsers.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// How much of an item's extracted text to keep as a preview for listings.
pub const PREVIEW_CHARS: usize = 200;

/// One entry within a transcript: a user prompt, an assistant message, a
/// tool call/result, or a session-level marker. Parsers never fail on a
/// single malformed or unrecognized item — they downgrade it to
/// `role: "other"` instead, so one bad line never hides the rest of a
/// transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionItem {
    /// Position within the transcript, 0-based.
    pub index: usize,
    /// `user`, `assistant`, `tool`, `system`, or `other` when the harness
    /// used a role we don't recognize.
    pub role: String,
    /// Finer-grained shape: `text`, `tool_use`, `tool_result`, `function_call`, ...
    pub kind: String,
    /// First `PREVIEW_CHARS` characters of the extracted text, for listings.
    pub text_preview: String,
    /// The complete extracted text this item's `text_preview` and
    /// `tokens_est` are derived from — memory only works if the actual
    /// content survives past parse time, not just a 200-char summary.
    /// Skipped when serializing: `ngd`'s `/api/transcript` sends
    /// `SessionItem` straight to the UI, which only ever renders
    /// `text_preview`, so shipping the full text there would needlessly
    /// bloat every transcript payload. Rust consumers (e.g. the watcher in
    /// `ng-adapters`) read this field directly, in-process, where the size
    /// cost doesn't apply.
    #[serde(skip_serializing, default)]
    pub text_full: String,
    /// Rough token estimate (bytes/4) of `text_full`, not just the preview
    /// — this is what context budgeting reads.
    pub tokens_est: i64,
    /// This item's own timestamp (Unix epoch seconds), extracted from the
    /// harness's per-item `timestamp` field where one exists (currently
    /// Claude Code and Codex). `None` when the format has no per-item
    /// timestamp or the field couldn't be parsed — callers (the watcher)
    /// fall back to the transcript file's mtime in that case, same as
    /// before this field existed. Skipped when serializing for the same
    /// reason as `text_full`: `ngd`'s `/api/transcript` payload doesn't
    /// need it yet, and Rust consumers read the field directly, in-process.
    #[serde(skip_serializing, default)]
    pub timestamp: Option<i64>,
    /// 1-based line number in the source file, for formats where a rewrite
    /// can target a single line (JSONL). `None` for formats stored as one
    /// JSON document or one file per message, which `rewrite.rs` cannot
    /// address at line granularity.
    pub raw_line: Option<usize>,
}

/// Metadata about a discovered session, before its items are loaded. Cheap
/// to produce for every session on disk — `discover_sessions` builds only
/// this, `load_transcript` does the (potentially expensive) parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub harness: String,
    pub path: PathBuf,
    pub project: Option<String>,
    pub modified_at: SystemTime,
    /// Cheap hint about item count (e.g. line count), so a UI can size a
    /// list without parsing every session up front.
    pub items_hint: Option<usize>,
}

/// A fully parsed session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub info: SessionInfo,
    pub items: Vec<SessionItem>,
    /// Lines that were present in the source file but could not be parsed
    /// as JSON at all. Counted, never silently dropped without a trace.
    pub skipped: usize,
}
