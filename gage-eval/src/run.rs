//! Run one test: spawn Claude Code with the Gage MCP server attached
//! and record what landed on disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::eval::{Root, Test};
use crate::scanner;
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
    /// Ad hoc evals dir (`--evals-dir`), when not the in-repo default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evals_dir: Option<String>,
    /// Judge model, recorded when the run includes scanner tests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
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

pub struct BatchConfig<'a> {
    pub model: &'a str,
    pub effort: &'a str,
    pub note: Option<&'a str>,
    pub root: &'a Root,
    /// Ad hoc evals dir to record in the manifest; `None` for the
    /// in-repo default.
    pub evals_dir: Option<&'a Path>,
    /// Concurrent samples within a scanner test.
    pub jobs: usize,
    pub judge_model: &'a str,
}

pub fn run_batch(
    tests: &[&Test],
    config: &BatchConfig<'_>,
    mut on_event: impl FnMut(Event<'_>),
) -> io::Result<RunResult> {
    let run_id = gage_core::uuid::new_uuid();
    let started_at = now_iso();
    let names: Vec<String> = tests.iter().map(|t| t.id()).collect();

    // Runs execute in a short-pathed /tmp workspace and are copied to
    // ~/.gage/evals on finish; see the storage module doc for why the
    // short path matters. A failed run leaves the workspace behind.
    let run = storage::workspace_dir(&run_id);
    fs::create_dir_all(&run)?;
    let gage_bin = sibling_gage_bin()?;

    // Prompt tests need a shared claude home with the Gage plugin
    // installed; scanner tests spawn agents through `gage scan`, which
    // manages its own claude setup.
    let prompt_env = if tests.iter().any(|t| !t.is_scanner()) {
        let claude_home = storage::prepare_claude_home(&run, config.model, config.effort)?;
        let claude_bin = find_claude().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "`claude` binary not on PATH")
        })?;
        install_gage_plugin(&claude_bin, &claude_home, &run)?;
        Some((claude_bin, claude_home))
    } else {
        None
    };

    let has_scanner = tests.iter().any(|t| t.is_scanner());
    let manifest = Manifest {
        run_id: run_id.clone(),
        started_at: started_at.clone(),
        finished_at: None,
        model: config.model.to_string(),
        effort: config.effort.to_string(),
        test_names: names.clone(),
        note: config.note.map(str::to_string),
        evals_dir: config.evals_dir.map(|p| p.to_string_lossy().into_owned()),
        judge_model: has_scanner.then(|| config.judge_model.to_string()),
    };
    write_manifest(&run, &manifest)?;

    for test in tests {
        let name = test.id();
        on_event(Event::Started(&name));
        let (exit_code, score) = if test.is_scanner() {
            fs::create_dir_all(storage::test_dir(&run, &name))?;
            write_test_json(&run, test)?;
            let score = scanner::run_test(
                &run,
                test,
                config.root,
                &gage_bin,
                config.jobs,
                config.judge_model,
            )?;
            (0, Some(score))
        } else {
            let (claude_bin, claude_home) = prompt_env
                .as_ref()
                .expect("prepared when prompt tests exist");
            let max_turns = test.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
            let exit_code = run_one(&run, test, max_turns, claude_bin, claude_home, config.root)?;
            (exit_code, score::score_test(&run, test)?)
        };
        on_event(Event::TestFinished {
            name: &name,
            exit_code,
            score,
        });
    }

    write_manifest(
        &run,
        &Manifest {
            finished_at: Some(now_iso()),
            ..manifest
        },
    )?;
    storage::archive_run(&run, &run_id)?;

    Ok(RunResult { run_id })
}

/// Stage the Gage plugin marketplace under the run dir and install it
/// into the shared `claude_home`. Mirrors what `gage init` does, but
/// scoped entirely to this eval's uuid dir.
fn install_gage_plugin(claude_bin: &Path, claude_home: &Path, run: &Path) -> io::Result<()> {
    let marketplace = storage::plugin_marketplace_dir(run);
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
    run: &Path,
    test: &Test,
    max_turns: u32,
    claude_bin: &Path,
    claude_home: &Path,
    root: &Root,
) -> io::Result<i32> {
    let cwd = storage::prepare_test(run, &test.id())?;
    fs::write(cwd.join("CLAUDE.md"), RULES_MD)?;
    write_test_json(run, test)?;

    let gage_home = storage::test_gage_home(run, &test.id());
    fs::create_dir_all(&gage_home)?;
    if let Some(sql) = &test.db_init {
        seed_db(&gage_home, sql)?;
    }
    let projects_dir = match &test.fixture {
        Some(name) => root.fixture_projects_dir(name),
        None => {
            let p = storage::test_empty_projects(run, &test.id());
            fs::create_dir_all(&p)?;
            p
        }
    };

    let stdout = fs::File::create(storage::stdout_path(run, &test.id()))?;
    let stderr = fs::File::create(storage::stderr_path(run, &test.id()))?;
    let mut cmd = Command::new(claude_bin);
    cmd.arg("-p")
        .arg(test.prompt.as_deref().expect("validated prompt test"))
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
        .env("CLAUDE_PROJECTS_DIR", &projects_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()?;

    let exit_code = status.code().unwrap_or(-1);
    if exit_code != 0 {
        fs::write(
            storage::error_exit_code_path(run, &test.id()),
            exit_code.to_string(),
        )?;
    }
    Ok(exit_code)
}

fn seed_db(gage_home: &Path, sql: &str) -> io::Result<()> {
    let db_path = gage_home.join("data").join("gage.db");
    let conn = gage_db::db::open_db_at(&db_path).map_err(io::Error::other)?;
    conn.execute_batch(sql).map_err(io::Error::other)
}

fn write_test_json(run: &Path, test: &Test) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(test).map_err(io::Error::other)?;
    fs::write(storage::test_json_path(run, &test.id()), bytes)
}

/// Resolve the `gage` binary sitting next to the currently-running
/// `gage-eval` binary. The plugin's MCP server invokes this.
pub fn sibling_gage_bin() -> io::Result<PathBuf> {
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

fn write_manifest(run: &Path, manifest: &Manifest) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    fs::write(storage::manifest_path(run), bytes)
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
