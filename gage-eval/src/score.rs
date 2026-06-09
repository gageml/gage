//! Score a finished test: run every `expect` regex against
//! `output.txt` and persist the result as `score.json`. All patterns
//! must match for the test to pass.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::eval::Test;
use crate::storage;
use gage_claude::session_reader::SessionReader;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    pub passed: bool,
    pub matches: Vec<MatchResult>,
    /// Number of assistant turns observed in the session. `None` when no
    /// session was found (e.g. claude died before writing anything).
    #[serde(default)]
    pub turns: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub pattern: String,
    pub matched: bool,
}

/// Run every regex from `test.expect` against `output.txt`. Returns
/// `None` if the test has no `expect`. Otherwise writes `score.json`
/// and returns the `Score`. Errors if `expect` is present but yields
/// no patterns, or if any pattern fails to compile.
pub fn score_test(run_id: &str, test: &Test) -> io::Result<Option<Score>> {
    let expect = match &test.expect {
        Some(e) => e,
        None => return Ok(None),
    };
    let patterns = expect.patterns();
    if patterns.is_empty() && expect.db_rows.is_empty() && expect.max_turns.is_none() {
        return Err(io::Error::other(format!(
            "test `{}` has `expect` but no checks (set `match`, `match_all`, `db_rows`, or `max_turns`)",
            test.id()
        )));
    }
    let output = fs::read_to_string(storage::stdout_path(run_id, &test.id())).unwrap_or_default();

    let mut matches = Vec::with_capacity(patterns.len() + 2);
    for pat in patterns {
        let re = Regex::new(&pat).map_err(io::Error::other)?;
        matches.push(MatchResult {
            matched: re.is_match(&output),
            pattern: pat,
        });
    }

    for sql in &expect.db_rows {
        let (rows, label) = match run_db_rows(run_id, &test.id(), sql) {
            Ok(n) => (n, format!("db: returned {n} rows")),
            Err(e) => (0, format!("db: {e}")),
        };
        matches.push(MatchResult {
            pattern: format!("{sql}\n  -> {label}"),
            matched: rows > 0,
        });
    }

    let turns = match storage::session_path(run_id, &test.id()) {
        Some(p) => Some(count_turns(&p)?),
        None => None,
    };
    if let Some(max) = expect.max_turns {
        let actual = turns.unwrap_or(0);
        matches.push(MatchResult {
            pattern: format!("turns <= {max} (was {actual})"),
            matched: actual <= max,
        });
    }

    // The run is only a success if `claude` exited cleanly. A non-zero
    // exit (e.g. `Reached max turns`) means the run did not complete as
    // intended, so the test fails regardless of the other checks.
    let exit_code = read_exit_code(run_id, &test.id());
    matches.push(MatchResult {
        pattern: format!("exit code == 0 (was {exit_code})"),
        matched: exit_code == 0,
    });

    let score = Score {
        passed: matches.iter().all(|m| m.matched),
        matches,
        turns,
    };
    let bytes = serde_json::to_vec_pretty(&score).map_err(io::Error::other)?;
    fs::write(storage::score_path(run_id, &test.id()), bytes)?;
    Ok(Some(score))
}

/// Read the recorded claude exit code for a test. `run_one` writes the
/// `ERROR_EXIT_CODE` file only on a non-zero exit, so an absent or
/// unparseable file means a clean exit (`0`).
fn read_exit_code(run_id: &str, test_id: &str) -> i32 {
    fs::read_to_string(storage::error_exit_code_path(run_id, test_id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Run `sql` against the test's `gage.db` and return the row count.
/// Errors when the db file is missing or the query is invalid.
fn run_db_rows(run_id: &str, test_id: &str, sql: &str) -> io::Result<u32> {
    let db_path = storage::test_gage_home(run_id, test_id)
        .join("data")
        .join("gage.db");
    if !db_path.exists() {
        return Err(io::Error::other(format!(
            "db not found at {}",
            db_path.display()
        )));
    }
    let conn = gage_db::db::open_db_at(&db_path);
    let mut stmt = conn.prepare(sql).map_err(io::Error::other)?;
    let mut rows = stmt.query([]).map_err(io::Error::other)?;
    let mut count: u32 = 0;
    while rows.next().map_err(io::Error::other)?.is_some() {
        count += 1;
    }
    Ok(count)
}

/// Count assistant turns in a session JSONL: the number of distinct
/// `message.id` values across `assistant`-typed entries. Matches the
/// turn numbering used by the markdown report's session outline.
pub fn count_turns(path: &Path) -> io::Result<u32> {
    let mut seen: HashSet<String> = HashSet::new();
    for item in SessionReader::open(path)? {
        let (_line, value) = item?;
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(id) = value.pointer("/message/id").and_then(Value::as_str) else {
            continue;
        };
        seen.insert(id.to_string());
    }
    Ok(u32::try_from(seen.len()).unwrap_or(u32::MAX))
}

/// Read a previously-written `score.json`. Returns `None` if absent.
pub fn read_score(run_id: &str, test_name: &str) -> io::Result<Option<Score>> {
    let path = storage::score_path(run_id, test_name);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let score: Score = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(Some(score))
}
