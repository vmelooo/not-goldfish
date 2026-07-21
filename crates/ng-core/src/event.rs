//! The captured event model.

use serde::{Deserialize, Serialize};

/// Maximum stored content size per event (256 KiB). Content beyond this is
/// truncated with an explicit marker so the model never mistakes a partial
/// payload for a full one.
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// One captured item from a harness session: a user prompt, a tool output,
/// an assistant message, a session marker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Harness session identifier.
    pub session_id: String,
    /// Absolute project path (cwd of the session).
    pub project: String,
    /// Which harness produced it: `claude-code`, `opencode`, `kimi`, ...
    pub harness: String,
    /// Kind of event: `prompt`, `tool_output`, `assistant`, `session_start`, `session_end`.
    pub kind: String,
    /// Free-form text content (already truncated to `MAX_CONTENT_BYTES`).
    pub content: String,
    /// Space-separated lexical tags extracted at capture time.
    #[serde(default)]
    pub tags: String,
    /// Structured capture metadata as a JSON string (tool name, file_path,
    /// transcript_path...). `None` for events captured before this field
    /// existed and for kinds that carry none. `#[serde(default)]` keeps the
    /// socket protocol compatible in both directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
    /// Unix epoch seconds.
    pub created_at: i64,
}

impl Event {
    /// Truncate content to the storage cap, appending an explicit marker
    /// when data was cut so provenance stays honest.
    pub fn cap_content(mut self) -> Self {
        if self.content.len() > MAX_CONTENT_BYTES {
            let mut cut = MAX_CONTENT_BYTES;
            while !self.content.is_char_boundary(cut) {
                cut -= 1;
            }
            let total = self.content.len();
            self.content.truncate(cut);
            self.content
                .push_str(&format!("\n[ng: truncated, original {} bytes]", total));
        }
        self
    }

    /// Rough token estimate (~4 bytes per token) used for context budgeting.
    pub fn tokens_est(&self) -> i64 {
        (self.content.len() / 4) as i64
    }
}
