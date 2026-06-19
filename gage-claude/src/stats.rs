//! Per-session statistics derived from a session JSONL on demand.
//!
//! These calculations are not cached: callers (e.g. `gage session list
//! --stats`) pay the parse cost each time. Adding caching here is a
//! deliberate non-goal — keep the state model simple.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use chrono::DateTime;
use serde_json::Value;

use crate::session_reader::SessionReader;

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    /// Number of distinct assistant turns, identified by `message.id`.
    pub turn_count: u64,
    /// Total tokens through the model. Sum of input, output,
    /// cache_creation, and cache_read; the four Anthropic usage buckets
    /// are disjoint, so the sum is a count-once total.
    pub total_tokens: u64,
    /// Wall time the model was actively generating. Sum of
    /// inter-entry gaps whose next entry is an assistant message.
    /// Excludes tool execution time, permission waits, and time
    /// between turns.
    pub model_time_ms: u64,
}

/// Walk the session JSONL once and compute all three stats.
pub fn compute_session_stats(path: &Path) -> io::Result<SessionStats> {
    let reader = SessionReader::open(path)?;
    let mut turns = TurnCounter::new();
    let mut total_tokens: i64 = 0;
    let mut model_time_ms: i64 = 0;
    let mut prev_ts: Option<i64> = None;
    let mut max_turn: usize = 0;

    for item in reader {
        let (_, entry) = match item {
            Ok(p) => p,
            Err(_) => continue,
        };

        if let Some(n) = turns.observe(&entry) {
            max_turn = max_turn.max(n);
        }

        if entry.get("type").and_then(Value::as_str) == Some("assistant")
            && let Some(usage) = entry.get("message").and_then(|m| m.get("usage"))
        {
            for k in [
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ] {
                total_tokens += usage.get(k).and_then(Value::as_i64).unwrap_or(0);
            }
        }

        let ts_ms = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis());
        if let Some(ts) = ts_ms {
            if let Some(prev) = prev_ts
                && is_model_generation(&entry)
            {
                let delta = ts - prev;
                if delta > 0 {
                    model_time_ms += delta;
                }
            }
            prev_ts = Some(ts);
        }
    }

    Ok(SessionStats {
        turn_count: max_turn as u64,
        total_tokens: total_tokens.max(0) as u64,
        model_time_ms: model_time_ms.max(0) as u64,
    })
}

/// True when this entry is an assistant message — i.e. the preceding
/// inter-entry gap is time the model spent generating. Gaps ending
/// in anything else (tool_result, human prompt, system event,
/// attachment) are tool execution, permission waits, or out-of-band
/// latency and are excluded from model time.
fn is_model_generation(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("assistant")
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
