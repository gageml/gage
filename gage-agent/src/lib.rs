//! Run Claude Code in an isolated home for Gage workflows.
//!
//! Two entry points share the same isolated-home setup:
//!
//! - [`run`] runs the interactive `gage agent` command: it spawns
//!   `claude`, inherits the terminal, and mirrors session JSONLs live so
//!   a SIGKILL'd run still leaves a viewable session.
//! - [`spawn_judge`] runs the same setup non-interactively via
//!   `claude -p <prompt>` for the scanner path, returning an
//!   [`AgentSession`] the caller drives with `wait`/`kill`.
//!
//! Both assemble a throwaway run dir at `~/.gage/tmp/<run_id>/` containing
//! `cwd/` (the empty working directory Claude runs in) and `claude/` (the
//! `CLAUDE_CONFIG_DIR`), seed an isolated home, and install the gage
//! plugin. The non-interactive scanner path additionally builds a scan
//! sandbox under the run dir — a filtered sqlite db at `db/gage.sqlite`
//! containing only the rows associated with the scan, and a
//! `projects/` tree of hardlinks to the session JSONLs in the scan's
//! `scan_session` set — and points the child claude at them via
//! `GAGE_DB` and `CLAUDE_PROJECTS_DIR`. The MCP server resolves those
//! env vars on first tool call, so the judge process physically cannot
//! see rows or sessions outside its scan.
//!
//! The session JSONL Claude writes is hardlinked into a caller-supplied
//! archive dir under `~/.gage/claude/<name>/` (`default` for the
//! interactive command, the scanner name for the scanner path); because
//! a hardlink shares the inode, that archived view stays current and
//! survives the run dir's removal on cleanup, so `gage session -A`
//! reads it without a copy step.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gage_claude::plugin;
use gage_core::config::gage_home;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::io::AsyncReadExt;
use tokio::process::{Child as TokioChild, Command as TokioCommand};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Grace period for the `SIGTERM` → `SIGKILL` shutdown `wait` performs when
/// its timeout elapses.
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// Default [`AgentSession::wait`] timeout when [`JudgeOpts::timeout`] is unset.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

pub fn run(name: Option<String>, prompt: Option<String>) -> io::Result<ExitStatus> {
    let PreparedRun {
        run_dir,
        cwd,
        claude_home,
        archive_dir,
        claude_bin,
    } = prepare_run(agent_archive_dir(name.as_deref().unwrap_or("default")))?;
    let user_projects = user_claude_projects()?;
    let projects_dir = claude_home.join("projects");

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

    let mut cmd = Command::new(&claude_bin);
    cmd.current_dir(&cwd)
        .env("CLAUDE_CONFIG_DIR", &claude_home)
        .env("CLAUDE_PROJECTS_DIR", &user_projects)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1");
    if let Some(prompt) = &prompt {
        cmd.arg(prompt);
    }
    let status = cmd.status();

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

    Ok(status)
}

/// Options for [`spawn_judge`], mirroring the `call_agent` Rune opts.
pub struct JudgeOpts {
    /// Agent identifier; selects the archive dir `~/.gage/claude/<name>/`.
    pub name: String,
    /// `--model` passed to `claude`, if set.
    pub model: Option<String>,
    /// `--max-turns` passed to `claude`, if set.
    pub max_turns: Option<u32>,
    /// Wait timeout in seconds applied by [`AgentSession::wait`]. `None`
    /// uses [`DEFAULT_WAIT_TIMEOUT`].
    pub timeout: Option<usize>,
    /// The scan run this judge serves. Drives the sandbox build: a
    /// filtered sqlite db and a filtered `projects/` tree get
    /// materialized under the judge's run dir, and the child claude is
    /// pointed at them via `GAGE_DB` and `CLAUDE_PROJECTS_DIR`.
    pub scan_id: String,
}

/// A spawned non-interactive judge `claude` process and the run state its
/// exit needs. Drive it with [`wait`](AgentSession::wait) (normal path) or
/// [`kill`](AgentSession::kill) (forced shutdown). Dropping it SIGKILLs the
/// child (`kill_on_drop`), so a session abandoned mid-run does not leak.
pub struct AgentSession {
    child: TokioChild,
    stdout: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    mirror: Option<SessionMirror>,
    output: Option<AgentOutput>,
    run_dir: PathBuf,
    cwd: PathBuf,
    claude_home: PathBuf,
    archive_dir: PathBuf,
    timeout: Option<usize>,
}

/// The result of a completed [`AgentSession`].
#[derive(Clone)]
pub struct AgentOutput {
    /// Process exit status.
    pub status: ExitStatus,
    /// Captured stdout — the final assistant message from `claude -p`.
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
}

/// Set up an isolated judge home and spawn `claude -p <prompt>`
/// non-interactively, with stdout and stderr piped so the caller can read
/// the agent's output. The blocking setup (home seeding, plugin install)
/// runs on a blocking thread.
pub async fn spawn_judge(prompt: &str, opts: &JudgeOpts) -> io::Result<AgentSession> {
    let archive_dir = agent_archive_dir(&opts.name);
    let scan_id = opts.scan_id.clone();
    let (prep, sandbox) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let prep = prepare_run(archive_dir)?;
        let sandbox = build_scan_sandbox(&prep.run_dir, &scan_id)?;
        Ok((prep, sandbox))
    })
    .await
    .map_err(io::Error::other)??;

    let mut cmd = TokioCommand::new(&prep.claude_bin);
    cmd.arg("-p").arg(prompt);
    cmd.args(["--thinking-display", "summarized"]);
    if let Some(model) = &opts.model {
        cmd.arg("--model").arg(model);
    }
    if let Some(max_turns) = opts.max_turns {
        cmd.arg("--max-turns").arg(max_turns.to_string());
    }
    cmd.current_dir(&prep.cwd)
        .env("CLAUDE_CONFIG_DIR", &prep.claude_home)
        .env("CLAUDE_PROJECTS_DIR", &sandbox.projects_dir)
        .env("GAGE_DB", &sandbox.db_path)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;
    let stdout = spawn_reader(child.stdout.take());
    let stderr = spawn_reader(child.stderr.take());

    // Hardlink the session JSONL into the archive dir the moment Claude
    // creates it, so an abnormal exit (timeout, kill) still leaves a
    // viewable record well before the caller's wait timeout elapses.
    let projects_dir = prep.claude_home.join("projects");
    let mirror = match start_session_mirror(&projects_dir, &prep.archive_dir) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("warning: session mirror watcher failed to start: {e}");
            None
        }
    };

    Ok(AgentSession {
        child,
        stdout,
        stderr,
        mirror,
        output: None,
        run_dir: prep.run_dir,
        cwd: prep.cwd,
        claude_home: prep.claude_home,
        archive_dir: prep.archive_dir,
        timeout: opts.timeout,
    })
}

impl AgentSession {
    /// OS process id while the child is running; `None` once it has exited.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// The completed output, available after a successful [`wait`](Self::wait).
    /// `None` before the child has exited. Cloneable and re-readable any
    /// number of times.
    pub fn output(&self) -> Option<AgentOutput> {
        self.output.clone()
    }

    /// Await the child's exit, bounded by the timeout configured in
    /// [`JudgeOpts::timeout`] (defaulting to [`DEFAULT_WAIT_TIMEOUT`]). On
    /// the first call, drain the captured output, remove the run dir, and
    /// cache the result. Subsequent calls return the cached output without
    /// repeating that work. On timeout, the child is shut down gracefully
    /// (`SIGTERM`, then `SIGKILL` after [`TIMEOUT_GRACE`]) and the run dir
    /// is cleaned up before a `TimedOut` error is returned, so the caller
    /// need not handle process teardown. The session JSONL is hardlinked
    /// into the archive dir continuously by the mirror watcher, so a
    /// timed-out run still leaves a viewable record.
    pub async fn wait(&mut self) -> io::Result<AgentOutput> {
        if let Some(output) = &self.output {
            return Ok(output.clone());
        }

        let timeout = self
            .timeout
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(DEFAULT_WAIT_TIMEOUT);
        let status = match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                // Shut the still-running child down gracefully and clean up
                // before surfacing the timeout, so the ubiquitous
                // `wait(t).await?` pattern never leaves a process running.
                self.terminate(TIMEOUT_GRACE).await?;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "agent timeout"));
            }
        };

        let stdout = join_reader(self.stdout.take()).await?;
        let stderr = join_reader(self.stderr.take()).await?;

        self.stop_mirror();
        archive_sessions(&self.claude_home, &self.archive_dir)?;
        cleanup_run_dir(&self.run_dir, &self.cwd);

        let output = AgentOutput {
            status,
            stdout,
            stderr,
        };
        self.output = Some(output.clone());
        Ok(output)
    }

    /// Send `SIGTERM`, wait up to `grace` for the child to exit, then
    /// `SIGKILL` if it has not. The session JSONL is hardlinked after the
    /// process is gone, and the run dir is removed once `cwd/` is empty.
    pub async fn kill(&mut self, grace: Duration) -> io::Result<()> {
        self.terminate(grace).await
    }

    /// Graceful shutdown shared by [`kill`](Self::kill) and `wait`'s timeout
    /// path: `SIGTERM`, wait up to `grace`, then `SIGKILL`, then stop the
    /// mirror, run the final archive sweep, and remove the run dir.
    async fn terminate(&mut self, grace: Duration) -> io::Result<()> {
        if let Some(pid) = self.child.id() {
            // SAFETY: pid identifies a child process this struct owns and
            // has not yet reaped; SIGTERM is a valid signal number.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => {
                status?;
            }
            Err(_) => self.child.kill().await?,
        }

        self.stop_mirror();
        archive_sessions(&self.claude_home, &self.archive_dir)?;
        cleanup_run_dir(&self.run_dir, &self.cwd);
        Ok(())
    }

    /// Stop the mirror watcher before the final archive sweep and run-dir
    /// removal. A no-op if it never started or was already stopped.
    fn stop_mirror(&mut self) {
        if let Some(mirror) = self.mirror.take() {
            mirror.stop();
        }
    }
}

/// Read a piped child stream to end on a background task. Returns `None`
/// when the stream was not captured.
fn spawn_reader<R>(stream: Option<R>) -> Option<JoinHandle<io::Result<Vec<u8>>>>
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    stream.map(|mut stream| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            Ok(buf)
        })
    })
}

/// Await a reader task started by [`spawn_reader`], flattening the join
/// error and the read error. An absent task yields empty output.
async fn join_reader(handle: Option<JoinHandle<io::Result<Vec<u8>>>>) -> io::Result<Vec<u8>> {
    match handle {
        Some(handle) => handle.await.map_err(io::Error::other)?,
        None => Ok(Vec::new()),
    }
}

/// The isolated-home state shared by both judge entry points, produced by
/// [`prepare_run`].
struct PreparedRun {
    run_dir: PathBuf,
    cwd: PathBuf,
    claude_home: PathBuf,
    archive_dir: PathBuf,
    claude_bin: PathBuf,
}

/// Paths a scan sandbox installs under a run dir. The judge child reads
/// these via `GAGE_DB` and `CLAUDE_PROJECTS_DIR`.
struct ScanSandbox {
    db_path: PathBuf,
    projects_dir: PathBuf,
}

/// Build a per-scan sandbox under `run_dir`: a filtered sqlite db at
/// `<run_dir>/db/gage.sqlite` and a `projects/` tree of hardlinks to the
/// session JSONLs the scan selected. Falls back to a one-shot copy when
/// hardlinking would cross filesystems (`EXDEV`).
fn build_scan_sandbox(run_dir: &Path, scan_id: &str) -> io::Result<ScanSandbox> {
    let db_path = run_dir.join("db").join("gage.sqlite");
    let projects_dir = run_dir.join("projects");
    fs::create_dir_all(&projects_dir)?;

    gage_db::sandbox::materialize_scan_sandbox(&gage_db::db::db_path(), &db_path, scan_id)
        .map_err(io::Error::other)?;

    let scope = scan_session_ids(scan_id)?;
    let source_projects = user_claude_projects()?;
    link_in_scope_sessions(&source_projects, &projects_dir, &scope)?;

    Ok(ScanSandbox {
        db_path,
        projects_dir,
    })
}

/// Look up the session ids associated with `scan_id` in the canonical
/// gage db.
fn scan_session_ids(scan_id: &str) -> io::Result<std::collections::HashSet<String>> {
    let conn = gage_db::db::open_db().map_err(io::Error::other)?;
    let ids = gage_db::scan::session_ids_for_scan(&conn, scan_id).map_err(io::Error::other)?;
    Ok(ids.into_iter().collect())
}

/// Walk `source_projects/*/<uuid>.jsonl` and hardlink each file whose
/// stem is in `scope` to the mirrored path under `dest_projects/`. The
/// project-subdir layout is preserved so `SessionListBuilder` reads the
/// sandbox the same way it reads the real corpus. A missing source
/// projects dir yields an empty sandbox (the scan may select nothing the
/// judge needs).
fn link_in_scope_sessions(
    source_projects: &Path,
    dest_projects: &Path,
    scope: &std::collections::HashSet<String>,
) -> io::Result<()> {
    let projects_iter = match fs::read_dir(source_projects) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for project in projects_iter {
        let project = project?.path();
        if !project.is_dir() {
            continue;
        }
        let project_name = match project.file_name() {
            Some(n) => n,
            None => continue,
        };
        let dest_project = dest_projects.join(project_name);
        let mut created_dest = false;
        for file in fs::read_dir(&project)? {
            let file = file?.path();
            if file.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = match file.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s,
                None => continue,
            };
            if !scope.contains(stem) {
                continue;
            }
            if !created_dest {
                fs::create_dir_all(&dest_project)?;
                created_dest = true;
            }
            let dest = dest_project.join(file.file_name().unwrap());
            link_or_copy(&file, &dest)?;
        }
    }
    Ok(())
}

/// Assemble the throwaway run dir, seed the isolated home, and install the
/// gage plugin. `archive_dir` is where session JSONLs get hardlinked.
fn prepare_run(archive_dir: PathBuf) -> io::Result<PreparedRun> {
    let run_id = Uuid::new_v4().to_string();
    let run_dir = tmp_run_dir(&run_id);
    let cwd = run_dir.join("cwd");
    let claude_home = run_dir.join("claude");
    let marketplace = claude_home.join(".plugin-marketplace");

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

    Ok(PreparedRun {
        run_dir,
        cwd,
        claude_home,
        archive_dir,
        claude_bin,
    })
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
        "mcp__plugin_gage_gage__IssueOpen".into(),
        "mcp__plugin_gage_gage__NoteDoc".into(),
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

fn agent_archive_dir(name: &str) -> PathBuf {
    gage_home().join("claude").join(name)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::tempdir;

    #[test]
    fn link_in_scope_sessions_picks_only_scope_ids() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        let project = source.join("-Users-x-proj");
        fs::create_dir_all(&project).unwrap();
        let in_scope = "11111111-1111-1111-1111-111111111111";
        let out_of_scope = "22222222-2222-2222-2222-222222222222";
        fs::write(project.join(format!("{in_scope}.jsonl")), b"a").unwrap();
        fs::write(project.join(format!("{out_of_scope}.jsonl")), b"b").unwrap();
        // A stray non-jsonl file in the project dir is ignored.
        fs::write(project.join("README.md"), b"x").unwrap();

        let mut scope = HashSet::new();
        scope.insert(in_scope.to_string());
        fs::create_dir_all(&dest).unwrap();
        link_in_scope_sessions(&source, &dest, &scope).unwrap();

        let linked = dest.join("-Users-x-proj").join(format!("{in_scope}.jsonl"));
        assert!(linked.exists(), "in-scope session not linked");
        assert!(
            !dest
                .join("-Users-x-proj")
                .join(format!("{out_of_scope}.jsonl"))
                .exists(),
            "out-of-scope session was linked"
        );
    }

    #[test]
    fn link_in_scope_sessions_handles_missing_source() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let scope: HashSet<String> = HashSet::new();
        link_in_scope_sessions(&dir.path().join("absent"), &dest, &scope).unwrap();
    }

    #[test]
    fn link_in_scope_sessions_skips_empty_project_dirs() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        let project = source.join("-Users-x-empty");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest).unwrap();
        let scope = HashSet::new();
        link_in_scope_sessions(&source, &dest, &scope).unwrap();
        assert!(
            !dest.join("-Users-x-empty").exists(),
            "empty project dir was created in dest"
        );
    }
}
