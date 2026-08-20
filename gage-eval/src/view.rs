//! Resolve run-id prefixes to eval runs.

use std::io;

use crate::storage::{self, RunSummary};

/// Resolve a run-id prefix to exactly one `RunSummary`. Errors with a
/// table of matches when ambiguous, with `NotFound` when nothing
/// matches.
pub fn resolve(prefix: &str) -> io::Result<RunSummary> {
    let runs = storage::list_runs()?;
    let matches: Vec<RunSummary> = runs
        .into_iter()
        .filter(|r| r.run_id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no run matches prefix `{prefix}`"),
        )),
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        _ => Err(io::Error::other(AmbiguousError { matches })),
    }
}

/// Carries the matched runs so the CLI can render them in the same
/// table style as `gage eval list`.
pub struct AmbiguousError {
    pub matches: Vec<RunSummary>,
}

impl std::fmt::Debug for AmbiguousError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmbiguousError")
            .field("matches", &self.matches.len())
            .finish()
    }
}

impl std::fmt::Display for AmbiguousError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} runs match prefix", self.matches.len())
    }
}

impl std::error::Error for AmbiguousError {}
