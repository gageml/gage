//! Structured results for an eval run — `results.json` at the run
//! root, aggregated from the per-test artifacts (test.json, score.json,
//! streams). Written when a run finishes; built on demand for runs
//! recorded before this file existed. The TUI renders this directly.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::eval::Test;
use crate::score::{self, Score};
use crate::storage;

/// Bumped when the structure gains fields older files lack; `ensure`
/// rebuilds any file with an older version.
const VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct Results {
    #[serde(default)]
    pub version: u32,
    pub tests: Vec<TestResult>,
}

#[derive(Serialize, Deserialize)]
pub struct TestResult {
    /// Test id, `{eval}/{test}`
    pub name: String,
    /// Pass/fail; None when the test was not scored
    pub passed: Option<bool>,
    /// Score checks in evaluation order
    pub checks: Vec<Check>,
    /// Assistant turns observed in the session (prompt tests)
    pub turns: Option<u32>,
    pub exit_code: i32,
    /// Prompt test input; None for scanner tests
    pub prompt: Option<String>,
    /// Scanner test configuration
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scanners: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples: Option<u32>,
    pub output: String,
    pub stderr: String,
    /// Session the test's claude wrote, when one was recorded
    pub session_id: Option<String>,
    /// Every session the test produced, in display order: the prompt
    /// test's agent session, or per sample the scan agent sessions and
    /// the judge session
    #[serde(default)]
    pub sessions: Vec<SessionRef>,
}

#[derive(Serialize, Deserialize)]
pub struct SessionRef {
    /// `agent` or `judge`
    pub kind: String,
    /// Sample number, for scanner-test sessions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<u32>,
    pub id: String,
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Check {
    pub label: String,
    pub passed: bool,
}

/// Read the run's results, building and persisting them from the
/// per-test artifacts when `results.json` is absent (runs recorded
/// before it existed) or written at an older structure version.
pub fn ensure(run_dir: &Path) -> io::Result<Results> {
    let path = storage::results_path(run_dir);
    if path.exists() {
        let bytes = fs::read(&path)?;
        let results: Results = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if results.version == VERSION {
            return Ok(results);
        }
    }
    write(run_dir)
}

/// Build the results from the run's artifacts and write `results.json`.
pub fn write(run_dir: &Path) -> io::Result<Results> {
    let results = build(run_dir)?;
    let bytes = serde_json::to_vec_pretty(&results).map_err(io::Error::other)?;
    fs::write(storage::results_path(run_dir), bytes)?;
    Ok(results)
}

fn build(run_dir: &Path) -> io::Result<Results> {
    let mut names = storage::test_names(run_dir)?;
    names.sort();
    let mut tests = Vec::with_capacity(names.len());
    for name in names {
        tests.push(build_test(run_dir, &name)?);
    }
    Ok(Results {
        version: VERSION,
        tests,
    })
}

fn build_test(run_dir: &Path, name: &str) -> io::Result<TestResult> {
    let test: Test = read_json(&storage::test_json_path(run_dir, name))?;
    let score: Option<Score> = score::read_score(run_dir, name)?;
    let checks = score
        .as_ref()
        .map(|s| {
            s.matches
                .iter()
                .map(|m| Check {
                    label: m.pattern.clone(),
                    passed: m.matched,
                })
                .collect()
        })
        .unwrap_or_default();
    let session_id = storage::session_path(run_dir, name)
        .and_then(|p| Some(p.file_stem()?.to_str()?.to_string()));
    let sessions = test_sessions(run_dir, name, &test);
    Ok(TestResult {
        name: name.to_string(),
        passed: score.as_ref().map(|s| s.passed),
        checks,
        turns: score.as_ref().and_then(|s| s.turns),
        exit_code: read_exit_code(run_dir, name),
        prompt: test.prompt.clone(),
        scanners: test.scanners.clone(),
        fixture: test.fixture.clone(),
        samples: test.is_scanner().then_some(test.samples),
        output: read_stream(&storage::stdout_path(run_dir, name)),
        stderr: read_stream(&storage::stderr_path(run_dir, name)),
        session_id,
        sessions,
    })
}

/// Enumerate the test's sessions from the run's artifacts. A prompt
/// test has its one claude session; a scanner test has, per sample,
/// the sandbox's scan agent sessions and the judge session.
fn test_sessions(run_dir: &Path, name: &str, test: &Test) -> Vec<SessionRef> {
    if !test.is_scanner() {
        return storage::session_path(run_dir, name)
            .and_then(|p| session_ref("agent", None, p))
            .into_iter()
            .collect();
    }
    let mut out = Vec::new();
    for sample in 1..=test.samples {
        let dir = storage::sample_dir(run_dir, name, sample);
        let agents_root = dir.join("gage-home").join("claude");
        for agent_dir in subdirs(&agents_root) {
            for jsonl in jsonl_files(&agent_dir) {
                out.extend(session_ref("agent", Some(sample), jsonl));
            }
        }
        for jsonl in jsonl_files(&dir.join("judge-sessions")) {
            out.extend(session_ref("judge", Some(sample), jsonl));
        }
    }
    out
}

fn session_ref(kind: &str, sample: Option<u32>, path: PathBuf) -> Option<SessionRef> {
    let id = path.file_stem()?.to_str()?.to_string();
    Some(SessionRef {
        kind: kind.to_string(),
        sample,
        id,
        path,
    })
}

/// Directory listing helpers for best-effort artifact discovery: a
/// sample that failed early has no sandbox or judge dirs, so an
/// unreadable directory reads as "no sessions here".
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Absent stream files read as empty: a test that never ran claude
/// (e.g. a scanner test) has no stdout/stderr capture.
fn read_stream(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Absent or unparseable means a clean exit; `run_one` writes the file
/// only on a non-zero exit (see `score::read_exit_code`).
fn read_exit_code(run_dir: &Path, name: &str) -> i32 {
    fs::read_to_string(storage::error_exit_code_path(run_dir, name))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
