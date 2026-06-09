//! Token accounting shared between the report renderer and the list
//! view. Walks a session JSONL and sums `message.usage` across entries.

use std::path::Path;

use gage_claude::session_reader::SessionReader;
use serde_json::Value;

#[derive(Default, Clone, Copy)]
pub struct Tokens {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

impl std::ops::AddAssign for Tokens {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.cached += rhs.cached;
        self.output += rhs.output;
    }
}

/// Extract token usage from an entry. Assistant entries carry
/// `message.usage`; other entries return all zeros. `input` folds
/// `input_tokens + cache_creation_input_tokens` (both billed at the
/// uncached input rate, more or less).
pub fn entry_tokens(value: &Value) -> Tokens {
    let usage = match value.pointer("/message/usage") {
        Some(u) => u,
        None => return Tokens::default(),
    };
    let get = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    Tokens {
        input: get("input_tokens") + get("cache_creation_input_tokens"),
        cached: get("cache_read_input_tokens"),
        output: get("output_tokens"),
    }
}

/// Sum tokens across all entries in a session JSONL. Returns
/// `Tokens::default()` if the file can't be read.
pub fn session_tokens(path: &Path) -> Tokens {
    let Ok(reader) = SessionReader::open(path) else {
        return Tokens::default();
    };
    let mut total = Tokens::default();
    for item in reader.flatten() {
        total += entry_tokens(&item.1);
    }
    total
}

pub fn fmt_tokens(t: &Tokens) -> Option<String> {
    if t.input == 0 && t.cached == 0 && t.output == 0 {
        return None;
    }
    Some(format!(
        "{} in / {} cached / {} out",
        format_count(t.input),
        format_count(t.cached),
        format_count(t.output)
    ))
}

pub fn format_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
