//! On-disk layout for eval runs.
//!
//! ```text
//! ~/.gage/evals/
//! └── {run-uuid}/
//!     ├── manifest.json              # run-level
//!     ├── claude-home/               # CLAUDE_CONFIG_DIR shared by all tests
//!     ├── plugin-marketplace/        # staged Gage plugin (installed into claude-home)
//!     └── results/
//!         └── {eval-name}/           # `/` in name becomes nested dir
//!             ├── test.json          # serialized toml entry
//!             ├── score.json         # pass/fail (only when expect is set)
//!             ├── output.txt         # claude stdout
//!             ├── error.txt          # claude stderr
//!             ├── ERROR_EXIT_CODE    # bare int, only when claude exits non-zero
//!             └── cwd/               # empty cwd for the claude spawn
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gage_core::config::gage_home;

pub fn evals_root() -> PathBuf {
    gage_home().join("evals")
}

pub fn run_dir(run_id: &str) -> PathBuf {
    evals_root().join(run_id)
}

pub fn test_dir(run_id: &str, test_name: &str) -> PathBuf {
    run_dir(run_id).join("results").join(test_name)
}

pub fn test_cwd(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("cwd")
}

/// Per-test `GAGE_HOME`. Empty dir; gage tools populate it on demand.
pub fn test_gage_home(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("gage-home")
}

/// Per-test empty `projects/` dir used as `GAGE_PROJECTS_DIR` when a
/// test specifies no fixture.
pub fn test_empty_projects(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("projects")
}

/// Shared across every test in a run. Holds settings.json, the
/// installed-plugins index, sessions, etc.
pub fn claude_home(run_id: &str) -> PathBuf {
    run_dir(run_id).join("claude-home")
}

/// Marketplace dir for the staged Gage plugin (per-run).
pub fn plugin_marketplace_dir(run_id: &str) -> PathBuf {
    run_dir(run_id).join("plugin-marketplace")
}

pub fn manifest_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("manifest.json")
}

pub fn test_json_path(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("test.json")
}

pub fn stdout_path(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("output.txt")
}

pub fn stderr_path(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("error.txt")
}

pub fn score_path(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("score.json")
}

pub fn error_exit_code_path(run_id: &str, test_name: &str) -> PathBuf {
    test_dir(run_id, test_name).join("ERROR_EXIT_CODE")
}

/// Locate the session JSONL claude wrote for this test. Each test runs
/// in its own per-test cwd, so claude creates exactly one projects
/// subdir containing exactly one `.jsonl` whose `cwd` field matches
/// `test_cwd(run_id, test_name)`. Returns `None` if no matching session
/// exists (e.g. claude failed before writing anything).
pub fn session_path(run_id: &str, test_name: &str) -> Option<PathBuf> {
    let projects = claude_home(run_id).join("projects");
    let expected_cwd = test_cwd(run_id, test_name);
    let projects_iter = fs::read_dir(&projects).ok()?;
    for entry in projects_iter.flatten() {
        let subdir = entry.path();
        if !subdir.is_dir() {
            continue;
        }
        let files = fs::read_dir(&subdir).ok()?;
        for file in files.flatten() {
            let p = file.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if first_entry_cwd(&p).as_deref() == Some(expected_cwd.to_string_lossy().as_ref()) {
                return Some(p);
            }
        }
    }
    None
}

fn first_entry_cwd(jsonl: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = fs::File::open(jsonl).ok()?;
    let reader = std::io::BufReader::new(f);
    for line in reader.lines().map_while(Result::ok) {
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
            return Some(c.to_string());
        }
    }
    None
}

/// Create the per-test directory layout and return the empty cwd path.
pub fn prepare_test(run_id: &str, test_name: &str) -> io::Result<PathBuf> {
    let cwd = test_cwd(run_id, test_name);
    fs::create_dir_all(&cwd)?;
    Ok(cwd)
}

/// Create the shared claude-home and seed `settings.json` with the
/// model + effort + thinking config every spawn uses.
pub fn prepare_claude_home(run_id: &str, model: &str, effort: &str) -> io::Result<PathBuf> {
    let home = claude_home(run_id);
    fs::create_dir_all(&home)?;
    let settings = serde_json::json!({
        "model": model,
        "effort": effort,
        "showThinkingSummaries": true,
    });
    fs::write(
        home.join("settings.json"),
        serde_json::to_vec_pretty(&settings).map_err(io::Error::other)?,
    )?;
    Ok(home)
}

/// List past run UUIDs, newest first.
pub fn list_runs() -> io::Result<Vec<RunSummary>> {
    let root = evals_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<RunSummary> = fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let id = path.file_name()?.to_string_lossy().into_owned();
            let manifest = manifest_path(&id);
            let manifest = read_manifest(&manifest).ok()?;
            let started_at_ms = chrono::DateTime::parse_from_rfc3339(&manifest.started_at)
                .ok()?
                .timestamp_millis();
            let (scored, passed) =
                manifest
                    .test_names
                    .iter()
                    .fold(
                        (0usize, 0usize),
                        |(scored, passed), n| match read_score_result(&id, n) {
                            None => (scored, passed),
                            Some(true) => (scored + 1, passed + 1),
                            Some(false) => (scored + 1, passed),
                        },
                    );
            let duration_ms = manifest.finished_at.as_ref().and_then(|f| {
                let end = chrono::DateTime::parse_from_rfc3339(f).ok()?;
                let start = chrono::DateTime::parse_from_rfc3339(&manifest.started_at).ok()?;
                Some((end - start).num_milliseconds())
            });
            let mut tokens = crate::tokens::Tokens::default();
            for n in &manifest.test_names {
                if let Some(p) = session_path(&id, n) {
                    tokens += crate::tokens::session_tokens(&p);
                }
            }
            Some(RunSummary {
                run_id: id,
                started_at_ms,
                total: scored,
                passed,
                duration_ms,
                test_count: manifest.test_names.len(),
                note: manifest.note,
                tokens,
                model: manifest.model,
            })
        })
        .collect();
    entries.sort_by(|a, b| b.started_at_ms.cmp(&a.started_at_ms));
    Ok(entries)
}

pub struct RunSummary {
    pub run_id: String,
    pub started_at_ms: i64,
    /// Number of scored tests (tests with a `score.json`). Tests
    /// without an `expect` clause are not counted.
    pub total: usize,
    pub passed: usize,
    /// Total wall-clock duration of the run. `None` if the run never
    /// recorded a `finished_at` (still running, crashed, or interrupted).
    pub duration_ms: Option<i64>,
    /// Total number of tests in the run (scored + unscored).
    pub test_count: usize,
    /// Optional user-supplied note recorded at run start.
    pub note: Option<String>,
    /// Sum of `message.usage` across every test's session JSONL.
    pub tokens: crate::tokens::Tokens,
    /// Model recorded in the run manifest, if any.
    pub model: Option<String>,
}

/// `None` if the test was not scored (no `expect` → no `score.json`).
/// `Some(true)` if scored and passed; `Some(false)` if scored and failed.
fn read_score_result(run_id: &str, test_name: &str) -> Option<bool> {
    let path = score_path(run_id, test_name);
    let bytes = fs::read(&path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("passed").and_then(serde_json::Value::as_bool)
}

#[derive(serde::Deserialize)]
struct Manifest {
    started_at: String,
    #[serde(default)]
    finished_at: Option<String>,
    test_names: Vec<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

fn read_manifest(path: &Path) -> io::Result<Manifest> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
