//! Run Claude Code in an isolated home for Gage workflows.
//!
//! Two entry points share one machinery:
//!
//! - [`run`] runs the interactive `gage agent` command: it spawns
//!   `claude`, inherits the terminal, and mirrors session JSONLs live so
//!   a SIGKILL'd run still leaves a viewable session.
//! - [`spawn_agent`] runs the same setup non-interactively via
//!   `claude -p <prompt>` for scanners and other automated callers,
//!   returning an [`AgentSession`] the caller drives with `wait`/`kill`.
//!
//! Both assemble a throwaway run dir at `~/.gage/tmp/<run_id>/` and
//! materialize an *agent sandbox* under it — a sqlite db at
//! `db/gage.sqlite` and a `projects/` tree of hardlinks to the user's
//! session JSONLs — then point the child `claude` at them via
//! `GAGE_DB` and `CLAUDE_PROJECTS_DIR`. The sandbox's rows and sessions
//! are determined by the caller's [`SandboxSpec`]; per-dimension `None`
//! means "everything," `Some(ids)` restricts. The MCP server in the
//! child resolves the two env vars on first tool call and physically
//! cannot see anything the sandbox does not contain.
//!
//! The sandbox also installs a trigger-driven writelog (see
//! `gage_db::sandbox`). On agent exit, `AgentSession::wait` /
//! [`run`] drains the writelog into the canonical gage db in a single
//! transaction before cleaning up. A replay failure preserves the run
//! dir so the user can inspect the sandbox state.
//!
//! The session JSONL Claude writes is hardlinked into a caller-supplied
//! archive dir under `~/.gage/claude/<name>/` (`default` for the
//! interactive command, the scanner name for the scanner path); because
//! a hardlink shares the inode, the archived view stays current and
//! survives run-dir removal, so `gage session -A` reads it without a
//! copy step.

use std::collections::HashSet;
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
pub use gage_db::sandbox::SandboxSpec;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::io::AsyncReadExt;
use tokio::process::{Child as TokioChild, Command as TokioCommand};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Canonical short names of every MCP tool the gage MCP server exposes.
/// Maintained by hand to mirror `gage-mcp`'s `TOOLS` registry. The order
/// here is the order resolved allowlists appear in `settings.json`, which
/// is purely cosmetic. This list is the universe `"*"` expands against in
/// [`ToolPolicy::tools`].
pub const TOOL_NAMES: &[&str] = &[
    "Query",
    "CommentWrite",
    "IssueList",
    "IssueGet",
    "IssueClose",
    "IssueComment",
    "IssueOpen",
];

/// Stateless helpers that produce resolved MCP-tool allowlists for an
/// [`AgentBuilder`]. Every function returns the final `Vec<String>` of
/// short names that gets written verbatim (with the
/// `mcp__plugin_gage_gage__` prefix) into the child claude's
/// `settings.json` `permissions.allow`.
pub struct ToolPolicy;

impl ToolPolicy {
    /// The shared baseline allowlist used by every entry point that gives
    /// a child claude access to gage data: the `gage agent` CLI and the
    /// Rune `call_agent` builder. Keeping a single source of truth here
    /// is load-bearing — the CLI is the user-visible reflection of what
    /// a `call_agent` child sees, and the two must not drift.
    pub fn default_tools() -> Vec<String> {
        vec!["Query".into()]
    }

    /// Resolve an allow/deny pair into a concrete allowlist. `"*"` in
    /// either list matches every name in [`TOOL_NAMES`]; every other
    /// entry must exactly match a known short name or this returns
    /// `Err` naming the offender. Result = ([`default_tools`] ∪ allow
    /// matches) − deny matches, preserving [`TOOL_NAMES`] order. The
    /// baseline is always included; remove a default with an explicit
    /// deny entry (e.g. `deny = ["Query"]`).
    ///
    /// [`default_tools`]: ToolPolicy::default_tools
    pub fn tools(allow: Vec<String>, deny: Vec<String>) -> Result<Vec<String>, String> {
        let mut allow_set = resolve(&allow)?;
        for d in Self::default_tools() {
            if let Some(n) = TOOL_NAMES.iter().find(|n| **n == d.as_str()) {
                allow_set.insert(*n);
            }
        }
        let deny_set = resolve(&deny)?;
        Ok(TOOL_NAMES
            .iter()
            .filter(|n| allow_set.contains(**n) && !deny_set.contains(**n))
            .map(|n| (*n).to_string())
            .collect())
    }
}

fn resolve(patterns: &[String]) -> Result<HashSet<&'static str>, String> {
    let mut out: HashSet<&'static str> = HashSet::new();
    for p in patterns {
        if p == "*" {
            out.extend(TOOL_NAMES.iter().copied());
            continue;
        }
        match TOOL_NAMES.iter().find(|n| **n == p.as_str()) {
            Some(n) => {
                out.insert(*n);
            }
            None => return Err(format!("unknown tool: {p}")),
        }
    }
    Ok(out)
}

/// Grace period for the `SIGTERM` → `SIGKILL` shutdown `wait` performs when
/// its timeout elapses.
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// Default [`AgentSession::wait`] timeout when no agent timeout was set.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// Fluent configuration for an [`Agent`]. All fields are optional;
/// `name` defaults to `"default"`, `sandbox` to the full-corpus spec,
/// `tools` to [`ToolPolicy::default_tools`] (Query only).
#[derive(Debug, Clone)]
pub struct AgentBuilder {
    name: Option<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    timeout: Option<usize>,
    sandbox: SandboxSpec,
    tools: Vec<String>,
    scan_id: Option<String>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self {
            name: None,
            model: None,
            max_turns: None,
            timeout: None,
            sandbox: SandboxSpec::default(),
            tools: ToolPolicy::default_tools(),
            scan_id: None,
        }
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Archive name; selects `~/.gage/claude/<name>/` for the session
    /// JSONL hardlinks. Defaults to `"default"`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// `--model` passed to the child claude.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// `--max-turns` passed to the child claude.
    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = Some(n);
        self
    }

    /// Wait timeout in seconds applied by [`AgentSession::wait`].
    pub fn timeout(mut self, secs: usize) -> Self {
        self.timeout = Some(secs);
        self
    }

    /// MCP tool allowlist (short names without the
    /// `mcp__plugin_gage_gage__` prefix). Replaces the default. Build
    /// the argument with [`ToolPolicy::tools`] for allow/deny semantics
    /// or [`ToolPolicy::default_tools`] for the Query-only baseline.
    pub fn tools(mut self, tools: Vec<String>) -> Self {
        self.tools = tools;
        self
    }

    /// Full sandbox filter spec — what rows and sessions the agent can
    /// see. Defaults to the unrestricted full-corpus spec.
    pub fn sandbox(mut self, sandbox: SandboxSpec) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Sugar for restricting the sandbox to a specific set of session
    /// ids. Overwrites any prior `sandbox().sessions` setting.
    pub fn sessions(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.sandbox.sessions = Some(ids.into_iter().collect());
        self
    }

    /// Scan id to expose to the child as `GAGE_SCAN_ID`. When set, the
    /// MCP server in the child links any notes or issues it creates to
    /// this scan via `scan_note` / `scan_issue`.
    pub fn scan_id(mut self, scan_id: impl Into<String>) -> Self {
        self.scan_id = Some(scan_id.into());
        self
    }

    pub fn build(self) -> Agent {
        Agent {
            name: self.name.unwrap_or_else(|| "default".to_string()),
            model: self.model,
            max_turns: self.max_turns,
            timeout: self.timeout,
            sandbox_spec: self.sandbox,
            tools: self.tools,
            scan_id: self.scan_id,
            state: None,
        }
    }
}

/// A configured but not-yet-running agent. Construct via [`AgentBuilder`].
///
/// The slow pre-spawn setup (plugin install + sandbox materialization)
/// runs inside [`Agent::init`]. Callers that want to wrap setup with a
/// progress indicator call `init` explicitly; [`Agent::run`] and
/// [`Agent::start_session`] call it themselves if it has not been run.
pub struct Agent {
    name: String,
    model: Option<String>,
    max_turns: Option<u32>,
    timeout: Option<usize>,
    sandbox_spec: SandboxSpec,
    tools: Vec<String>,
    scan_id: Option<String>,
    state: Option<RunState>,
}

/// Pre-spawn state owned by an [`Agent`] after [`Agent::init`] succeeds.
struct RunState {
    prep: PreparedRun,
    sandbox: Sandbox,
}

impl Agent {
    /// Run the slow pre-spawn setup: assemble the run dir, seed the
    /// isolated claude home, install the gage plugin, materialize the
    /// sandbox sqlite db, and hardlink the in-scope session JSONLs.
    /// Idempotent: a second call is a no-op.
    pub fn init(&mut self) -> io::Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let archive_dir = agent_archive_dir(&self.name);
        let prep = prepare_run(archive_dir, &self.tools)?;
        let sandbox = build_sandbox(&prep.run_dir, &self.sandbox_spec)?;
        self.state = Some(RunState { prep, sandbox });
        Ok(())
    }

    /// Spawn the child claude interactively (inherits stdio) and block
    /// until it exits. Calls [`Agent::init`] if not already done.
    pub fn run(mut self, prompt: Option<String>) -> io::Result<ExitStatus> {
        self.init()?;
        let RunState { prep, sandbox } = self.state.take().unwrap();
        run_interactive(prep, sandbox, self.model, self.scan_id, prompt)
    }

    /// Spawn the child claude non-interactively (stdio piped) via
    /// `claude -p <prompt>`. Returns an [`AgentSession`] the caller
    /// drives with `wait`/`kill`. Calls [`Agent::init`] on a blocking
    /// thread if not already done.
    pub async fn start_session(mut self, prompt: &str) -> io::Result<AgentSession> {
        if self.state.is_none() {
            let mut taken = self;
            let agent = tokio::task::spawn_blocking(move || -> io::Result<Agent> {
                taken.init()?;
                Ok(taken)
            })
            .await
            .map_err(io::Error::other)??;
            self = agent;
        }
        let RunState { prep, sandbox } = self.state.take().unwrap();
        start_session_inner(
            prep,
            sandbox,
            self.model,
            self.max_turns,
            self.timeout,
            self.scan_id,
            prompt,
        )
        .await
    }
}

fn run_interactive(
    prep: PreparedRun,
    sandbox: Sandbox,
    model: Option<String>,
    scan_id: Option<String>,
    prompt: Option<String>,
) -> io::Result<ExitStatus> {
    let projects_dir = prep.claude_home.join("projects");

    // Ignore SIGINT (and SIGQUIT) in the parent while Claude runs. Both
    // processes share the foreground process group, so the terminal still
    // delivers the signal to Claude; this just stops the parent from
    // tearing down before Claude exits and skipping the cleanup below.
    let prev_sigint = ignore_signal(libc::SIGINT);
    let prev_sigquit = ignore_signal(libc::SIGQUIT);

    let mirror = match start_session_mirror(&projects_dir, &prep.archive_dir) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("warning: session mirror watcher failed to start: {e}");
            None
        }
    };

    let mut cmd = Command::new(&prep.claude_bin);
    cmd.current_dir(&prep.cwd)
        .env("CLAUDE_CONFIG_DIR", &prep.claude_home)
        .env("CLAUDE_PROJECTS_DIR", &sandbox.projects_dir)
        .env("GAGE_DB", &sandbox.db_path)
        .env("GAGE_TOOLS", prep.tools.join(","))
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1");
    if let Some(id) = &scan_id {
        cmd.env("GAGE_SCAN_ID", id);
    }
    if let Some(model) = &model {
        cmd.arg("--model").arg(model);
    }
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

    let archived = archive_sessions(&prep.claude_home, &prep.archive_dir)?;
    for path in &archived {
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or("?"));
        println!("Saved agent session {session_id}");
    }

    let replay_ok = replay_and_report(&sandbox.db_path);
    if replay_ok {
        cleanup_run_dir(&prep.run_dir, &prep.cwd);
    } else {
        eprintln!(
            "warning: run dir preserved for inspection: {}",
            prep.run_dir.display()
        );
    }

    Ok(status)
}

/// Replay the sandbox's writelog into the canonical db. Returns `true`
/// on success or empty writelog; logs and returns `false` on failure so
/// the caller can preserve the run dir.
fn replay_and_report(sandbox_db: &Path) -> bool {
    match gage_db::sandbox::replay_writes(sandbox_db, &gage_db::db::db_path()) {
        Ok(0) => true,
        Ok(n) => {
            println!(
                "Replayed {n} agent write{} to main db",
                if n == 1 { "" } else { "s" }
            );
            true
        }
        Err(e) => {
            eprintln!("error: agent writeback failed: {e}");
            false
        }
    }
}

/// A spawned non-interactive agent `claude` process and the run state its
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
    sandbox_db: PathBuf,
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

/// Spawn the child claude non-interactively with the already-built run
/// state. Shared backend for [`Agent::start_session`].
async fn start_session_inner(
    prep: PreparedRun,
    sandbox: Sandbox,
    model: Option<String>,
    max_turns: Option<u32>,
    timeout: Option<usize>,
    scan_id: Option<String>,
    prompt: &str,
) -> io::Result<AgentSession> {
    let mut cmd = TokioCommand::new(&prep.claude_bin);
    cmd.arg("-p").arg(prompt);
    cmd.args(["--thinking-display", "summarized"]);
    if let Some(model) = &model {
        cmd.arg("--model").arg(model);
    }
    if let Some(max_turns) = max_turns {
        cmd.arg("--max-turns").arg(max_turns.to_string());
    }
    cmd.current_dir(&prep.cwd)
        .env("CLAUDE_CONFIG_DIR", &prep.claude_home)
        .env("CLAUDE_PROJECTS_DIR", &sandbox.projects_dir)
        .env("GAGE_DB", &sandbox.db_path)
        .env("GAGE_TOOLS", prep.tools.join(","))
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(id) = &scan_id {
        cmd.env("GAGE_SCAN_ID", id);
    }

    let mut child = cmd.spawn()?;
    let stdout = spawn_reader(child.stdout.take());
    let stderr = spawn_reader(child.stderr.take());

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
        sandbox_db: sandbox.db_path,
        timeout,
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
        self.finalize_run();

        let output = AgentOutput {
            status,
            stdout,
            stderr,
        };
        self.output = Some(output.clone());
        Ok(output)
    }

    /// Replay the sandbox writelog and clean up. Preserves the run dir
    /// when replay fails so the user can inspect the sandbox.
    fn finalize_run(&self) {
        if !replay_and_report(&self.sandbox_db) {
            eprintln!(
                "warning: run dir preserved for inspection: {}",
                self.run_dir.display()
            );
            return;
        }
        cleanup_run_dir(&self.run_dir, &self.cwd);
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
        self.finalize_run();
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
    /// Resolved MCP-tool allowlist (short names). Exported to the child
    /// claude as `GAGE_TOOLS` so the in-child gage MCP server registers
    /// only these tools.
    tools: Vec<String>,
}

/// Paths a sandbox installs under a run dir. The child claude reads
/// these via `GAGE_DB` and `CLAUDE_PROJECTS_DIR`.
struct Sandbox {
    db_path: PathBuf,
    projects_dir: PathBuf,
}

/// Build an agent sandbox under `run_dir`: a sqlite db at
/// `<run_dir>/db/gage.sqlite` materialized from the canonical db per
/// `spec`, and a `projects/` tree of hardlinks to the session JSONLs
/// the spec selects. Falls back to a one-shot copy when hardlinking
/// would cross filesystems (`EXDEV`).
fn build_sandbox(run_dir: &Path, spec: &SandboxSpec) -> io::Result<Sandbox> {
    let db_path = run_dir.join("db").join("gage.sqlite");
    let projects_dir = run_dir.join("projects");
    fs::create_dir_all(&projects_dir)?;

    gage_db::sandbox::materialize_sandbox(&gage_db::db::db_path(), &db_path, spec)
        .map_err(io::Error::other)?;

    let source_projects = user_claude_projects()?;
    link_sessions(&source_projects, &projects_dir, spec.sessions.as_ref())?;

    Ok(Sandbox {
        db_path,
        projects_dir,
    })
}

/// Walk `source_projects/*/<uuid>.jsonl` and hardlink each file into
/// the mirrored path under `dest_projects/`. When `scope` is `Some`,
/// only sessions whose id is in the set are linked; `None` links every
/// session. The project-subdir layout is preserved so
/// `SessionListBuilder` reads the sandbox the same way it reads the
/// real corpus. A missing source projects dir yields an empty sandbox.
fn link_sessions(
    source_projects: &Path,
    dest_projects: &Path,
    scope: Option<&HashSet<String>>,
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
            if let Some(set) = scope
                && !set.contains(stem)
            {
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
fn prepare_run(archive_dir: PathBuf, tools: &[String]) -> io::Result<PreparedRun> {
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
    seed_claude_home(&claude_home, &cwd, tools)?;

    let claude_bin = find_claude()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "`claude` binary not on PATH"))?;
    let gage_bin = sibling_gage_bin()?;
    install_gage_plugin(&claude_bin, &claude_home, &marketplace, &gage_bin, tools)?;

    Ok(PreparedRun {
        run_dir,
        cwd,
        claude_home,
        archive_dir,
        claude_bin,
        tools: tools.to_vec(),
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
    tools: &[String],
) -> io::Result<()> {
    plugin::write_plugin_files_to(marketplace, gage_bin)?;
    plugin::filter_tools_skill(marketplace, tools)?;
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
/// shift model behavior or what lands in the transcript. `tools` is the
/// MCP-tool allowlist verbatim — no entry is implicit.
fn seed_claude_home(claude_home: &Path, cwd: &Path, tools: &[String]) -> io::Result<()> {
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
    for key in ["theme", "tui"] {
        if let Some(v) = user_settings.as_ref().and_then(|s| s.get(key)) {
            settings.insert(key.into(), v.clone());
        }
    }
    let mut allow_set: Vec<String> = Vec::with_capacity(tools.len());
    for t in tools {
        let prefixed = format!("mcp__plugin_gage_gage__{t}");
        if !allow_set.contains(&prefixed) {
            allow_set.push(prefixed);
        }
    }
    let allow = serde_json::Value::Array(
        allow_set
            .into_iter()
            .map(serde_json::Value::String)
            .collect(),
    );
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

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn tool_policy_default_is_query_only() {
        assert_eq!(ToolPolicy::default_tools(), vec!["Query".to_string()]);
    }

    #[test]
    fn tool_policy_empty_allow_yields_defaults() {
        assert_eq!(
            ToolPolicy::tools(vec![], vec![]).unwrap(),
            ToolPolicy::default_tools()
        );
    }

    #[test]
    fn tool_policy_deny_can_remove_default() {
        assert!(ToolPolicy::tools(vec![], s(&["Query"])).unwrap().is_empty());
    }

    #[test]
    fn tool_policy_allow_extends_defaults() {
        let got = ToolPolicy::tools(s(&["IssueOpen"]), vec![]).unwrap();
        assert_eq!(got, s(&["Query", "IssueOpen"]));
    }

    #[test]
    fn tool_policy_star_allow_yields_all() {
        let got = ToolPolicy::tools(s(&["*"]), vec![]).unwrap();
        assert_eq!(got.len(), TOOL_NAMES.len());
        for n in TOOL_NAMES {
            assert!(got.iter().any(|g| g == n));
        }
    }

    #[test]
    fn tool_policy_star_minus_one() {
        let got = ToolPolicy::tools(s(&["*"]), s(&["IssueOpen"])).unwrap();
        assert_eq!(got.len(), TOOL_NAMES.len() - 1);
        assert!(!got.iter().any(|g| g == "IssueOpen"));
    }

    #[test]
    fn tool_policy_explicit_pair() {
        let got = ToolPolicy::tools(s(&["Query", "IssueGet"]), vec![]).unwrap();
        // Order follows TOOL_NAMES.
        assert_eq!(got, s(&["Query", "IssueGet"]));
    }

    #[test]
    fn tool_policy_star_deny_star_empty() {
        assert!(ToolPolicy::tools(s(&["*"]), s(&["*"])).unwrap().is_empty());
    }

    #[test]
    fn tool_policy_unknown_allow_errors() {
        let err = ToolPolicy::tools(s(&["IssueOpne"]), vec![]).unwrap_err();
        assert!(err.contains("IssueOpne"), "err = {err}");
    }

    #[test]
    fn tool_policy_unknown_deny_errors() {
        let err = ToolPolicy::tools(s(&["*"]), s(&["Bogus"])).unwrap_err();
        assert!(err.contains("Bogus"), "err = {err}");
    }

    #[test]
    fn link_sessions_picks_only_scope_ids() {
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
        link_sessions(&source, &dest, Some(&scope)).unwrap();

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
    fn link_sessions_all_links_everything() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        let project = source.join("-Users-x-proj");
        fs::create_dir_all(&project).unwrap();
        let id_a = "11111111-1111-1111-1111-111111111111";
        let id_b = "22222222-2222-2222-2222-222222222222";
        fs::write(project.join(format!("{id_a}.jsonl")), b"a").unwrap();
        fs::write(project.join(format!("{id_b}.jsonl")), b"b").unwrap();
        fs::create_dir_all(&dest).unwrap();
        link_sessions(&source, &dest, None).unwrap();
        assert!(
            dest.join("-Users-x-proj")
                .join(format!("{id_a}.jsonl"))
                .exists()
        );
        assert!(
            dest.join("-Users-x-proj")
                .join(format!("{id_b}.jsonl"))
                .exists()
        );
    }

    #[test]
    fn link_sessions_handles_missing_source() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let scope: HashSet<String> = HashSet::new();
        link_sessions(&dir.path().join("absent"), &dest, Some(&scope)).unwrap();
    }

    #[test]
    fn link_sessions_skips_empty_project_dirs() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        let project = source.join("-Users-x-empty");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&dest).unwrap();
        let scope = HashSet::new();
        link_sessions(&source, &dest, Some(&scope)).unwrap();
        assert!(
            !dest.join("-Users-x-empty").exists(),
            "empty project dir was created in dest"
        );
    }
}
