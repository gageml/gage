//! A hand-built rollout for this crate's tests: one turn of a
//! paginated session with a prompt, a reasoning summary, a shell
//! command, a patch and a failed MCP call under one script, and the
//! token records. Every id and path in it is made up.

use std::path::{Path, PathBuf};

use serde_json::Value;

pub(crate) const ROLLOUT: &str = include_str!("../testdata/rollout.jsonl");

/// The thread id the fixture records
pub(crate) const THREAD_ID: &str = "00000000-0000-7000-8000-000000000001";

/// The fixture's lines, parsed, in order: line `n` is at `n - 1`.
pub(crate) fn lines() -> Vec<Value> {
    ROLLOUT
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Write the fixture into the Codex home `home` as the rollout of
/// thread `id`, started by `thread_source`, and return its path.
pub(crate) fn seed(home: &Path, id: &str, thread_source: &str) -> PathBuf {
    let dir = home.join("sessions/2026/01/02");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-01-02T03-04-05-{id}.jsonl"));
    let text = ROLLOUT.replace(THREAD_ID, id).replace(
        r#""thread_source":"user""#,
        &format!(r#""thread_source":"{thread_source}""#),
    );
    std::fs::write(&path, text).unwrap();
    path
}
