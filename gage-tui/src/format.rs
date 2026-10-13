//! The content format this crate reads entries in directly.
//!
//! An entry's `raw` is the harness's own row, opaque by contract.
//! Three readers here open it anyway: the message pane renders its
//! content blocks, the turn counter groups assistant entries by the
//! message id inside it, and the outline labels entries by what the
//! driver derived from it. Each document is checked against the one
//! format those readers understand before any of them runs. Reading
//! `raw` this way is a decision to keep, not an accident; a second
//! format adds its own readers here or moves the work into the
//! driver.

use std::collections::HashMap;

use serde_json::Value;

/// The `content_format` whose `raw` entries this crate reads
pub const SUPPORTED: &str = "claude-jsonl 1";

/// Panics unless `content_format` is [`SUPPORTED`]. A session in
/// another format has no reader here, and rendering it would show
/// nothing or the wrong thing.
pub fn require_supported(session_id: &str, content_format: &str) {
    assert!(
        content_format == SUPPORTED,
        "session {session_id} has content format {content_format:?}; \
         this build reads {SUPPORTED:?} only"
    );
}

/// Assigns sequential 1-based turn numbers to assistant entries,
/// deduping by `message.id` so a multi-block assistant turn stays one
/// turn. Stateful across calls — the same instance must walk the whole
/// session in order.
#[derive(Default)]
pub struct TurnCounter {
    seen: HashMap<String, usize>,
    next: usize,
}

impl TurnCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the turn number for an assistant entry, or `None` for
    /// non-assistant entries and assistant entries without a
    /// `message.id`.
    pub fn observe(&mut self, entry: &Value) -> Option<usize> {
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let id = entry
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)?;
        Some(match self.seen.get(id) {
            Some(&n) => n,
            None => {
                self.next += 1;
                self.seen.insert(id.to_string(), self.next);
                self.next
            }
        })
    }
}
