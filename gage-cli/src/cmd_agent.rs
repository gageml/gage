//! `gage agent` — run Claude Code in an isolated home for Gage workflows.
//!
//! The judge subcommand assembles a throwaway run dir at
//! `~/.gage/tmp/<run_id>/` containing `cwd/` (the empty working directory
//! Claude runs in) and `claude/` (the `CLAUDE_CONFIG_DIR`). While Claude
//! runs, a watcher thread hardlinks each new session JSONL from
//! `<claude_home>/projects/*/<id>.jsonl` to
//! `~/.gage/claude/judge/<id>.jsonl`, so the archived view stays current
//! even if Claude is SIGKILLed. On exit a final sweep covers anything the
//! watcher missed and the tmp dir is removed if `cwd/` is empty. Anything
//! the user wrote in `cwd/` keeps the whole tmp dir around with a message
//! naming the path.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use clap::Subcommand;
use gage_claude::plugin;
use gage_core::config::gage_home;
use notify::{EventKind, RecursiveMode, Watcher};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Evaluate scanner evidence for issues
    Judge,
}

pub fn run(command: AgentCommand) {
    match command {
        AgentCommand::Judge => judge(),
    }
}

fn judge() {
    if let Err(e) = judge_inner() {
        eprintln!("gage agent judge: {e}");
        std::process::exit(1);
    }
}

fn judge_inner() -> io::Result<()> {
    let run_id = Uuid::new_v4().to_string();
    let run_dir = tmp_run_dir(&run_id);
    let cwd = run_dir.join("cwd");
    let claude_home = run_dir.join("claude");
    let marketplace = claude_home.join(".plugin-marketplace");
    let archive_dir = judge_archive_dir();

    let projects_dir = claude_home.join("projects");
    fs::create_dir_all(&claude_home)?;
    fs::create_dir_all(&cwd)?;
    fs::create_dir_all(&archive_dir)?;
    fs::create_dir_all(&projects_dir)?;
    seed_claude_home(&claude_home, &cwd)?;

    let claude_bin = find_claude()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "`claude` binary not on PATH"))?;
    let gage_bin = sibling_gage_bin()?;
    install_gage_plugin(&claude_bin, &claude_home, &marketplace, &gage_bin)?;

    // Ignore SIGINT (and SIGQUIT) in the parent while Claude runs. Both
    // processes share the foreground process group, so the terminal still
    // delivers the signal to Claude; this just stops the parent from
    // tearing down before Claude exits and skipping the cleanup below.
    let prev_sigint = ignore_signal(libc::SIGINT);
    let prev_sigquit = ignore_signal(libc::SIGQUIT);

    // Mirror session JSONLs into the archive dir as Claude creates them,
    // so a SIGKILL'd run still leaves a viewable session behind. The
    // watcher is dropped after Claude exits; a final sweep below covers
    // any file the watcher missed (e.g. created in the gap before the
    // watcher started).
    let mirror = match start_session_mirror(&projects_dir, &archive_dir) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("warning: session mirror watcher failed to start: {e}");
            None
        }
    };

    // The judge runs under an isolated CLAUDE_CONFIG_DIR, so the gage MCP
    // server it launches would resolve its corpus to this empty agent
    // home. Pin it to the user's real sessions via CLAUDE_PROJECTS_DIR
    // (Gage's session-corpus override; the `claude` binary uses
    // CLAUDE_CONFIG_DIR for its own session writes, so this does not
    // affect archiving).
    let user_projects = user_claude_projects()?;

    // The judge reads and writes a sandbox database seeded with only
    // neutral evidence (scanner notes), redirected via GAGE_DB. Every
    // reader — datafusion tables and MCP tools — resolves through
    // db_path(), so this one override isolates the model from issues,
    // commentary, and prior judgments without per-tool filtering.
    // GAGE_AGENT_JUDGE marks the mode for future skill/tool divergence.
    let sandbox_db = run_dir.join("gage.db");
    gage_db::db::create_judge_sandbox(&sandbox_db, &gage_db::db::db_path())
        .map_err(|e| io::Error::other(format!("create judge sandbox: {e}")))?;

    let status = Command::new(&claude_bin)
        .args(["-n", "gage:judge", "/gage:judge"])
        .current_dir(&cwd)
        .env("CLAUDE_CONFIG_DIR", &claude_home)
        .env("CLAUDE_PROJECTS_DIR", &user_projects)
        .env("GAGE_DB", &sandbox_db)
        .env("GAGE_AGENT_JUDGE", "1")
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .status();

    restore_signal(libc::SIGINT, prev_sigint);
    restore_signal(libc::SIGQUIT, prev_sigquit);

    if let Some(m) = mirror {
        m.stop();
    }

    let status = status?;

    let archived = archive_sessions(&claude_home, &archive_dir)?;
    for path in &archived {
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or("?"));
        println!("Saved agent session {session_id}");
    }
    if archived.is_empty() {
        println!("no session jsonl produced");
    }
    cleanup_run_dir(&run_dir, &cwd);

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Link every session JSONL Claude wrote under
/// `<claude_home>/projects/*/` into `<archive_dir>/<basename>`. The live
/// mirror watcher does the same work as files appear; this is a final
/// sweep to catch anything the watcher missed. Returns the destination
/// paths.
fn archive_sessions(claude_home: &Path, archive_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let projects = claude_home.join("projects");
    let mut out = Vec::new();
    let projects_iter = match fs::read_dir(&projects) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for project in projects_iter {
        let project = project?.path();
        if !project.is_dir() {
            continue;
        }
        for file in fs::read_dir(&project)? {
            let file = file?.path();
            if file.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(name) = file.file_name() else {
                continue;
            };
            let dest = archive_dir.join(name);
            link_or_copy(&file, &dest)?;
            out.push(dest);
        }
    }
    Ok(out)
}

/// Hardlink `src` to `dest`. Treats an existing `dest` as success (the
/// watcher already mirrored it). Falls back to a one-shot `fs::copy` when
/// the two paths are on different filesystems (`EXDEV`); that loses the
/// SIGKILL-safety guarantee for the affected file but keeps the archive
/// usable.
fn link_or_copy(src: &Path, dest: &Path) -> io::Result<()> {
    match fs::hard_link(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => fs::copy(src, dest).map(|_| ()),
        Err(e) => Err(e),
    }
}

/// Watcher handle. Calling `stop` drops the watcher (which closes the
/// event channel) then joins the worker thread.
struct SessionMirror {
    watcher: notify::RecommendedWatcher,
    thread: thread::JoinHandle<()>,
}

impl SessionMirror {
    fn stop(self) {
        drop(self.watcher);
        if self.thread.join().is_err() {
            eprintln!("warning: session mirror thread panicked");
        }
    }
}

/// Start a recursive watcher on `projects_dir` that hardlinks each new
/// `*.jsonl` into `archive_dir` as it appears. Also sweeps any files
/// already present at start time, in case Claude created a file before
/// the watcher was armed.
fn start_session_mirror(projects_dir: &Path, archive_dir: &Path) -> notify::Result<SessionMirror> {
    let (event_tx, event_rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(event_tx)?;
    watcher.watch(projects_dir, RecursiveMode::Recursive)?;

    // Initial sweep handles files created before the watcher armed.
    if let Err(e) = archive_sessions(projects_dir.parent().unwrap_or(projects_dir), archive_dir) {
        eprintln!("warning: initial session sweep failed: {e}");
    }

    let archive = archive_dir.to_path_buf();
    let thread = thread::spawn(move || {
        // recv returns Err when the watcher (and its event sender) is
        // dropped on stop. That's the normal exit path.
        while let Ok(ev) = event_rx.recv() {
            let Ok(ev) = ev else {
                continue;
            };
            if !matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                continue;
            }
            for path in ev.paths {
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(name) = path.file_name() else {
                    continue;
                };
                let dest = archive.join(name);
                if let Err(e) = link_or_copy(&path, &dest) {
                    eprintln!("warning: mirror link failed for {}: {e}", path.display());
                }
            }
        }
    });

    Ok(SessionMirror { watcher, thread })
}

/// Remove the tmp run dir unless the user wrote anything into `cwd/`. If
/// the cwd has files, leave the whole run dir in place and print its path
/// so the user can inspect or remove it.
fn cleanup_run_dir(run_dir: &Path, cwd: &Path) {
    // Ignore `.claude/` - Claude writes permission state there and it is not
    // user content worth preserving.
    let cwd_empty = match fs::read_dir(cwd) {
        Ok(it) => it
            .collect::<io::Result<Vec<_>>>()
            .map(|entries| !entries.iter().any(|e| e.file_name() != ".claude"))
            .unwrap_or(false),
        Err(_) => false,
    };
    if !cwd_empty {
        println!("cwd preserved (non-empty): {}", cwd.display());
        return;
    }
    if let Err(e) = fs::remove_dir_all(run_dir) {
        eprintln!(
            "warning: could not remove run dir {}: {e}",
            run_dir.display()
        );
    }
}

fn ignore_signal(sig: libc::c_int) -> libc::sighandler_t {
    // SAFETY: libc::signal is the documented BSD/POSIX-compatible API.
    // SIG_IGN is a valid handler value; the returned previous handler is
    // restored verbatim later.
    unsafe { libc::signal(sig, libc::SIG_IGN) }
}

fn restore_signal(sig: libc::c_int, prev: libc::sighandler_t) {
    // SAFETY: prev was returned by an earlier libc::signal call on the
    // same signal number.
    unsafe {
        libc::signal(sig, prev);
    }
}

fn install_gage_plugin(
    claude_bin: &Path,
    claude_home: &Path,
    marketplace: &Path,
    gage_bin: &Path,
) -> io::Result<()> {
    plugin::write_plugin_files_to(marketplace, gage_bin)?;
    plugin::write_marketplace_manifest_to(marketplace)?;
    claude_subcommand(
        claude_bin,
        claude_home,
        &[
            OsStr::new("plugin"),
            OsStr::new("marketplace"),
            OsStr::new("add"),
            marketplace.as_os_str(),
        ],
    )?;
    claude_subcommand(
        claude_bin,
        claude_home,
        &[
            OsStr::new("plugin"),
            OsStr::new("install"),
            OsStr::new("gage@gage"),
        ],
    )
}

fn claude_subcommand(claude_bin: &Path, claude_home: &Path, args: &[&OsStr]) -> io::Result<()> {
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
            "claude plugin setup failed with status {status}"
        )));
    }
    Ok(())
}

/// Populate the isolated home with the minimum needed to skip onboarding
/// and present a familiar UI, without inheriting any setting that could
/// shift model behavior or what lands in the transcript.
fn seed_claude_home(claude_home: &Path, cwd: &Path) -> io::Result<()> {
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME not set"))?;
    let user_claude = user_home.join(".claude");

    let user_creds = user_claude.join(".credentials.json");
    if user_creds.exists() {
        let target = claude_home.join(".credentials.json");
        std::os::unix::fs::symlink(&user_creds, &target)?;
    }

    let user_settings = read_json(&user_claude.join("settings.json"));
    let mut settings = serde_json::Map::new();
    settings.insert(
        "showThinkingSummaries".into(),
        serde_json::Value::Bool(true),
    );
    if let Some(theme) = user_settings.as_ref().and_then(|v| v.get("theme")) {
        settings.insert("theme".into(), theme.clone());
    }
    let allow = serde_json::Value::Array(vec![
        "mcp__plugin_gage_gage__Query".into(),
        "mcp__plugin_gage_gage__IssueList".into(),
        "mcp__plugin_gage_gage__IssueGet".into(),
    ]);
    let mut permissions = serde_json::Map::new();
    permissions.insert("allow".into(), allow);
    settings.insert("permissions".into(), serde_json::Value::Object(permissions));
    fs::write(
        claude_home.join("settings.json"),
        serde_json::to_vec_pretty(&serde_json::Value::Object(settings))
            .map_err(io::Error::other)?,
    )?;

    // Seed `.claude.json` (NB: parent of claude_home, not inside it) with
    // onboarding-completion + identity fields copied verbatim from the
    // user's real `.claude.json`, plus a `projects` entry for the empty
    // cwd marking it pre-trusted. Anything not in this whitelist stays
    // absent so Claude falls back to default behavior.
    let user_claude_json = read_json(&user_home.join(".claude.json")).unwrap_or_default();
    let whitelist = [
        "hasCompletedOnboarding",
        "lastOnboardingVersion",
        "migrationVersion",
        "userID",
        "oauthAccount",
        "firstStartTime",
        "claudeCodeFirstTokenDate",
        "installMethod",
    ];
    let mut claude_json = serde_json::Map::new();
    if let serde_json::Value::Object(map) = &user_claude_json {
        for key in whitelist {
            if let Some(v) = map.get(key) {
                claude_json.insert(key.to_string(), v.clone());
            }
        }
    }
    let mut project_entry = serde_json::Map::new();
    project_entry.insert(
        "hasTrustDialogAccepted".into(),
        serde_json::Value::Bool(true),
    );
    project_entry.insert(
        "hasCompletedProjectOnboarding".into(),
        serde_json::Value::Bool(true),
    );
    let mut projects = serde_json::Map::new();
    projects.insert(
        cwd.to_string_lossy().into_owned(),
        serde_json::Value::Object(project_entry),
    );
    claude_json.insert("projects".into(), serde_json::Value::Object(projects));
    fs::write(
        claude_home.join(".claude.json"),
        serde_json::to_vec_pretty(&serde_json::Value::Object(claude_json))
            .map_err(io::Error::other)?,
    )?;
    Ok(())
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The user's real Claude sessions directory (`$HOME/.claude/projects`),
/// independent of any redirected `CLAUDE_CONFIG_DIR`. This is the corpus
/// the judge analyzes.
fn user_claude_projects() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME not set"))?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

fn tmp_run_dir(run_id: &str) -> PathBuf {
    gage_home().join("tmp").join(run_id)
}

fn judge_archive_dir() -> PathBuf {
    gage_home().join("claude").join("judge")
}

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
