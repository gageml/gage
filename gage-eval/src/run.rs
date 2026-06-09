//! Run one test: spawn Claude Code with the Gage MCP server attached
//! and record what landed on disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use uuid::Uuid;

use crate::eval::Test;
use crate::score::{self, Score};
use crate::storage;
use gage_claude::plugin;

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub run_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub model: String,
    pub effort: String,
    pub test_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub struct RunResult {
    pub run_id: String,
}

/// Lifecycle event a caller can observe to drive a progress UI.
pub enum Event<'a> {
    Started(&'a str),
    TestFinished {
        name: &'a str,
        exit_code: i32,
        score: Option<Score>,
    },
}

const DEFAULT_MAX_TURNS: u32 = 5;

/// Project memory written to each test's cwd as `CLAUDE.md`. Claude Code loads
/// this in headless (`-p`) mode and honors it, which the
/// `--append-system-prompt` flag did not reliably do. The per-test cwd is a
/// throwaway staging dir whose path is visible to the agent; these rules stop
/// it from treating that path as a clue to the task.
const RULES_MD: &str = "\
# Rules

The current working directory is an empty, throwaway sandbox. It has no
relationship to the task. Do not inspect it, list it, read files in it,
or infer anything from its path or name when interpreting instructions.
Answer only from the tools and information the prompt provides.
";

pub fn run_batch(
    tests: &[&Test],
    model: &str,
    effort: &str,
    note: Option<&str>,
    mut on_event: impl FnMut(Event<'_>),
) -> io::Result<RunResult> {
    let run_id = Uuid::new_v4().to_string();
    let started_at = now_iso();
    let names: Vec<String> = tests.iter().map(|t| t.id()).collect();

    fs::create_dir_all(storage::run_dir(&run_id))?;
    let claude_home = storage::prepare_claude_home(&run_id, model, effort)?;
    let claude_bin = find_claude()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "`claude` binary not on PATH"))?;
    install_gage_plugin(&claude_bin, &claude_home, &run_id)?;
    let note = note.map(str::to_string);
    write_manifest(&Manifest {
        run_id: run_id.clone(),
        started_at: started_at.clone(),
        finished_at: None,
        model: model.to_string(),
        effort: effort.to_string(),
        test_names: names.clone(),
        note: note.clone(),
    })?;

    for test in tests {
        let name = test.id();
        on_event(Event::Started(&name));
        let max_turns = test.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
        let exit_code = run_one(&run_id, test, max_turns, &claude_bin, &claude_home)?;
        let score = score::score_test(&run_id, test)?;
        on_event(Event::TestFinished {
            name: &name,
            exit_code,
            score,
        });
    }

    write_manifest(&Manifest {
        run_id: run_id.clone(),
        started_at,
        finished_at: Some(now_iso()),
        model: model.to_string(),
        effort: effort.to_string(),
        test_names: names,
        note,
    })?;

    Ok(RunResult { run_id })
}

/// Stage the Gage plugin marketplace under the run dir and install it
/// into the shared `claude_home`. Mirrors what `gage init` does, but
/// scoped entirely to this eval's uuid dir.
fn install_gage_plugin(claude_bin: &Path, claude_home: &Path, run_id: &str) -> io::Result<()> {
    let marketplace = storage::plugin_marketplace_dir(run_id);
    let gage_bin = sibling_gage_bin()?;
    plugin::write_plugin_files_to(&marketplace, &gage_bin)?;
    plugin::write_marketplace_manifest_to(&marketplace)?;

    claude_subcommand(
        claude_bin,
        claude_home,
        &[
            "plugin",
            "marketplace",
            "add",
            &marketplace.to_string_lossy(),
        ],
    )?;
    claude_subcommand(claude_bin, claude_home, &["plugin", "install", "gage@gage"])?;
    Ok(())
}

fn claude_subcommand(claude_bin: &Path, claude_home: &Path, args: &[&str]) -> io::Result<()> {
    let status = Command::new(claude_bin)
        .args(args)
        .env("CLAUDE_CONFIG_DIR", claude_home)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "claude {args:?} failed with status {status}"
        )));
    }
    Ok(())
}

/// Run one test. Writes test.json, stdout.txt, stderr.txt, and (on
/// failure) ERROR_EXIT_CODE. Returns the claude exit code.
fn run_one(
    run_id: &str,
    test: &Test,
    max_turns: u32,
    claude_bin: &Path,
    claude_home: &Path,
) -> io::Result<i32> {
    let cwd = storage::prepare_test(run_id, &test.id())?;
    fs::write(cwd.join("CLAUDE.md"), RULES_MD)?;
    write_test_json(run_id, test)?;

    let gage_home = storage::test_gage_home(run_id, &test.id());
    fs::create_dir_all(&gage_home)?;
    if let Some(sql) = &test.db_init {
        seed_db(&gage_home, sql)?;
    }
    let projects_dir = match &test.fixture {
        Some(name) => crate::eval::fixture_projects_dir(name),
        None => {
            let p = storage::test_empty_projects(run_id, &test.id());
            fs::create_dir_all(&p)?;
            p
        }
    };

    let stdout = fs::File::create(storage::stdout_path(run_id, &test.id()))?;
    let stderr = fs::File::create(storage::stderr_path(run_id, &test.id()))?;
    let mut cmd = Command::new(claude_bin);
    cmd.arg("-p")
        .arg(&test.prompt)
        .arg("--max-turns")
        .arg(max_turns.to_string())
        // Counters Opus 4.7's server-side `display: "omitted"` default
        // so thinking content survives in the recorded session. See
        // scanners/hidden-thinking/enable-thinking.md.
        .arg("--thinking-display")
        .arg("summarized");
    if let Some(settings) = &test.claude {
        let json = serde_json::to_string(settings).map_err(io::Error::other)?;
        cmd.arg("--settings").arg(json);
    }
    let status = cmd
        .current_dir(&cwd)
        .env("CLAUDE_CONFIG_DIR", claude_home)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .env("GAGE_HOME", &gage_home)
        .env("GAGE_PROJECTS_DIR", &projects_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()?;

    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        fs::write(
            storage::error_exit_code_path(run_id, &test.id()),
            exit_code.to_string(),
        )?;
    }
    Ok(exit_code)
}

fn seed_db(gage_home: &Path, sql: &str) -> io::Result<()> {
    let db_path = gage_home.join("data").join("gage.db");
    let conn = gage_db::db::open_db_at(&db_path);
    conn.execute_batch(sql).map_err(io::Error::other)
}

fn write_test_json(run_id: &str, test: &Test) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(test).map_err(io::Error::other)?;
    fs::write(storage::test_json_path(run_id, &test.id()), bytes)
}

/// Resolve the `gage` binary sitting next to the currently-running
/// `gage-eval` binary. The plugin's MCP server invokes this.
fn sibling_gage_bin() -> io::Result<PathBuf> {
    std::env::current_exe()?
        .parent()
        .map(|p| p.join("gage"))
        .ok_or_else(|| io::Error::other("can't locate sibling `gage` binary"))
}

fn find_claude() -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join("claude");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn write_manifest(manifest: &Manifest) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    fs::write(storage::manifest_path(&manifest.run_id), bytes)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
