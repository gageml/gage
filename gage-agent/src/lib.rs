//! Run Claude Code in an isolated home for Gage workflows.
//!
//! Two entry points share one machinery:
//!
//! - [`Agent::run`] is the interactive `gage agent` command: spawns
//!   `claude`, inherits the terminal, and mirrors session JSONLs live
//!   so a SIGKILL'd run still leaves a viewable session.
//! - [`Agent::start_streaming_session`] is the scanner-driven path,
//!   spawning `claude -p` with `--input-format stream-json` /
//!   `--output-format stream-json` and a per-call HTTP MCP server.
//!   Returns a [`StreamingAgentSession`] the caller drives
//!   event-by-event via `recv_event`.
//! - [`Agent::run_print`] is the one-shot path: `claude -p` with no
//!   MCP server and no tools, output captured and returned (e.g. the
//!   eval judge).
//!
//! Both assemble a throwaway run dir at `~/.gage/tmp/<run_id>/` and
//! point the child `claude` at an isolated `CLAUDE_CONFIG_DIR` /
//! `CLAUDE_PROJECTS_DIR` inside it. Corpus access is MCP-mediated:
//! the child issues `Query` calls to the in-process gage MCP server,
//! which reads the canonical db through a per-agent DataFusion
//! context configured per tool (see `gage_mcp::ToolSpec`).
//!
//! The session JSONL Claude writes is hardlinked into a caller-supplied
//! archive dir under `~/.gage/claude/<name>/` (`default` for the
//! interactive command, the scanner name for the scanner path); because
//! a hardlink shares the inode, the archived view stays current and
//! survives run-dir removal, so `gage session -A` reads it without a
//! copy step.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gage_core::config::gage_home;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child as TokioChild, ChildStdin, Command as TokioCommand};
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Canonical short names of every MCP tool the gage MCP server exposes.
/// Maintained by hand to mirror `gage-mcp`'s `TOOLS` registry. The order
/// here is the order resolved allowlists appear in `settings.json`, which
/// is purely cosmetic. This list is the universe `"*"` expands against in
/// [`ToolPolicy::tools`].
pub const TOOL_NAMES: &[&str] = &[
    "Query",
    "IssueUpdate",
    "IssueComment",
    "IssueWrite",
    "NoteWrite",
];

/// Stateless helpers that produce resolved MCP-tool allowlists for an
/// [`AgentBuilder`]. Every function returns the final `Vec<String>` of
/// short names that gets written verbatim (with the `mcp__gage__`
/// prefix) into the child claude's `settings.json` `permissions.allow`.
pub struct ToolPolicy;

impl ToolPolicy {
    /// Resolve an allow/deny pair into a concrete allowlist. `"*"` in
    /// either list matches every name in [`TOOL_NAMES`]; every other
    /// entry must exactly match a known short name or this returns
    /// `Err` naming the offender. Result = (allow matches) − (deny
    /// matches), preserving [`TOOL_NAMES`] order. There is no implicit
    /// baseline — every tool the caller wants exposed must be listed.
    pub fn tools(allow: Vec<String>, deny: Vec<String>) -> Result<Vec<String>, String> {
        let allow_set = resolve(&allow)?;
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

/// Render the `--mcp-config` argument. Names the server `gage`,
/// producing the `mcp__gage__<Tool>` wire prefix.
///
/// The name must not start with `plugin_`: claude treats any MCP
/// server whose name has that prefix as plugin-installed and attaches
/// plugin-identity context (ambient skills get attributed to the
/// "gage plugin", etc.).
fn mcp_config_json(url: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"gage":{{"type":"http","url":"{}"}}}}}}"#,
        url.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// Grace period for the `SIGTERM` → `SIGKILL` shutdown `wait` performs when
/// its timeout elapses.
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// Default [`StreamingAgentSession::wait_exit`] timeout when no agent timeout was set.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// System prompt for a headless child claude. `Empty` (the default)
/// passes `--system-prompt ""` so the model runs without Claude Code's
/// interactive-assistant guidance (code style rules, tone, etc.),
/// which otherwise leaks into analysis output.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum SystemPrompt {
    /// `--system-prompt ""` — no system prompt at all.
    #[default]
    Empty,
    /// Omit the flag — the child uses Claude Code's default prompt.
    ClaudeDefault,
    /// `--system-prompt <s>` — replace with a caller-supplied prompt.
    Custom(String),
}

/// Fluent configuration for an [`Agent`]. All fields are optional;
/// `name` defaults to `"default"`; `tools` to empty (the caller must
/// list every tool to expose).
#[derive(Debug, Clone, Default)]
pub struct AgentBuilder {
    name: Option<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    timeout: Option<usize>,
    tools: Vec<String>,
    prompt: Option<String>,
    archive_dir: Option<PathBuf>,
    /// Streamable-HTTP MCP endpoint to wire as the child claude's MCP
    /// server via `--mcp-config`. When `None` the child runs with no
    /// MCP servers at all.
    mcp_url: Option<String>,
    system_prompt: SystemPrompt,
    /// `--append-system-prompt` value; composes with Claude Code's
    /// default prompt.
    system_prompt_append: Option<String>,
    /// Pass `--no-session-persistence` on the print path so the child
    /// writes no session JSONL and nothing is archived.
    no_session_persistence: bool,
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

    /// Explicit session archive dir, overriding the
    /// `~/.gage/claude/<name>/` default derived from `name`.
    pub fn archive_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.archive_dir = Some(dir.into());
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

    /// Wait timeout in seconds applied by [`StreamingAgentSession::wait_exit`].
    pub fn timeout(mut self, secs: usize) -> Self {
        self.timeout = Some(secs);
        self
    }

    /// MCP tool allowlist (short names without the
    /// `mcp__gage__` prefix). Replaces the default. Build
    /// the argument with [`ToolPolicy::tools`] for allow/deny semantics.
    /// There is no implicit baseline — every tool to expose must be
    /// listed.
    pub fn tools(mut self, tools: Vec<String>) -> Self {
        self.tools = tools;
        self
    }

    /// Initial prompt passed to the interactive child claude as its
    /// first user message. Ignored by the streaming path, which sends
    /// its prompt over stdin.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    /// Streamable-HTTP MCP URL the child claude should connect to.
    pub fn mcp_url(mut self, url: impl Into<String>) -> Self {
        self.mcp_url = Some(url.into());
        self
    }

    /// `--system-prompt` value, replacing the default empty prompt.
    pub fn system_prompt(mut self, s: impl Into<String>) -> Self {
        self.system_prompt = SystemPrompt::Custom(s.into());
        self
    }

    /// Use Claude Code's default system prompt (omit `--system-prompt`).
    pub fn default_system_prompt(mut self) -> Self {
        self.system_prompt = SystemPrompt::ClaudeDefault;
        self
    }

    /// `--append-system-prompt` value; implies the Claude Code default
    /// prompt, which the flag appends to.
    pub fn default_system_prompt_append(mut self, s: impl Into<String>) -> Self {
        self.system_prompt = SystemPrompt::ClaudeDefault;
        self.system_prompt_append = Some(s.into());
        self
    }

    /// Skip session persistence on [`Agent::run_print`]: the child
    /// writes no session JSONL and nothing is archived. For one-shot
    /// calls whose transcript has no value, such as the `/context`
    /// probe. The judge and other callers that read the archived
    /// session leave this off.
    pub fn no_session_persistence(mut self) -> Self {
        self.no_session_persistence = true;
        self
    }

    pub fn build(self) -> Agent {
        Agent {
            name: self.name.unwrap_or_else(|| "default".to_string()),
            model: self.model,
            max_turns: self.max_turns,
            timeout: self.timeout,
            tools: self.tools,
            prompt: self.prompt,
            mcp_url: self.mcp_url,
            archive_dir: self.archive_dir,
            system_prompt: self.system_prompt,
            system_prompt_append: self.system_prompt_append,
            no_session_persistence: self.no_session_persistence,
            prep: None,
        }
    }
}

/// A configured but not-yet-running agent. Construct via [`AgentBuilder`].
///
/// The slow pre-spawn setup (sandbox materialization) runs inside
/// [`Agent::init`]. Callers that want to wrap setup with a
/// progress indicator call `init` explicitly; [`Agent::run`] and
/// [`Agent::start_session`] call it themselves if it has not been run.
pub struct Agent {
    name: String,
    model: Option<String>,
    max_turns: Option<u32>,
    timeout: Option<usize>,
    tools: Vec<String>,
    prompt: Option<String>,
    mcp_url: Option<String>,
    archive_dir: Option<PathBuf>,
    system_prompt: SystemPrompt,
    system_prompt_append: Option<String>,
    no_session_persistence: bool,
    prep: Option<PreparedRun>,
}

impl Agent {
    /// Run the slow pre-spawn setup: assemble the run dir and seed the
    /// isolated claude home. Idempotent: a second call is a no-op.
    pub fn init(&mut self) -> io::Result<()> {
        if self.prep.is_some() {
            return Ok(());
        }
        let prep = prepare_run(self.archive_dir(), &self.tools)?;
        self.prep = Some(prep);
        Ok(())
    }

    fn archive_dir(&self) -> PathBuf {
        self.archive_dir
            .clone()
            .unwrap_or_else(|| agent_archive_dir(&self.name))
    }

    /// Spawn the child claude interactively (inherits stdio) and block
    /// until it exits. Calls [`Agent::init`] if not already done.
    pub fn run(mut self) -> io::Result<ExitStatus> {
        self.init()?;
        let prep = self.prep.take().unwrap();
        run_interactive(prep, self.model, self.mcp_url, self.prompt)
    }

    /// Spawn the child claude in print mode (`claude -p`) with no MCP
    /// server and no tools, block until it exits, and archive the session
    /// JSONL it wrote unless the builder set
    /// [`AgentBuilder::no_session_persistence`]. Returns the captured
    /// output.
    pub fn run_print(mut self, prompt: &str) -> io::Result<Output> {
        if self.prep.is_none() {
            self.prep = Some(prepare_run(self.archive_dir(), &self.tools)?);
        }
        let prep = self.prep.take().unwrap();
        run_print(
            prep,
            self.model,
            &self.system_prompt,
            &self.system_prompt_append,
            self.no_session_persistence,
            prompt,
        )
    }

    /// Spawn the child claude non-interactively with stream-json input
    /// and output, piped stdin held open for `send`/`stop`. Returns a
    /// [`StreamingAgentSession`] the caller drives event-by-event via
    /// `recv_event`. Calls [`Agent::init`] on a blocking thread if not
    /// already done.
    pub async fn start_streaming_session(
        mut self,
        prompt: &str,
    ) -> io::Result<StreamingAgentSession> {
        if self.prep.is_none() {
            let mut taken = self;
            let agent = tokio::task::spawn_blocking(move || -> io::Result<Agent> {
                taken.init()?;
                Ok(taken)
            })
            .await
            .map_err(io::Error::other)??;
            self = agent;
        }
        let prep = self.prep.take().unwrap();
        start_streaming_session_inner(
            prep,
            self.model,
            self.max_turns,
            self.timeout,
            self.mcp_url,
            self.system_prompt,
            self.system_prompt_append,
            prompt,
        )
        .await
    }
}

fn run_interactive(
    prep: PreparedRun,
    model: Option<String>,
    mcp_url: Option<String>,
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
    cmd.args(["--tools", "WaitForMcpServers"]);
    cmd.current_dir(&prep.cwd)
        .env("CLAUDE_CONFIG_DIR", &prep.claude_home)
        .env("CLAUDE_PROJECTS_DIR", &projects_dir)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .env("ENABLE_TOOL_SEARCH", "false");
    if let Some(url) = &mcp_url {
        cmd.arg("--mcp-config").arg(mcp_config_json(url));
    }
    // Only the --mcp-config server (if any) — never plugin-installed
    // servers or account-level claude.ai connectors.
    cmd.arg("--strict-mcp-config");
    // `user` = our seeded settings.json in CLAUDE_CONFIG_DIR
    // (theme, tui, showThinkingSummaries, permissions.allow).
    // Skip `project` / `local` so we don't pick up settings from
    // the cwd directory.
    cmd.arg("--setting-sources").arg("user");
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

    cleanup_run_dir(&prep.run_dir, &prep.cwd);
    Ok(status)
}

fn run_print(
    prep: PreparedRun,
    model: Option<String>,
    system_prompt: &SystemPrompt,
    system_prompt_append: &Option<String>,
    no_session_persistence: bool,
    prompt: &str,
) -> io::Result<Output> {
    let projects_dir = prep.claude_home.join("projects");
    let mut cmd = Command::new(&prep.claude_bin);
    cmd.args(["-p", prompt, "--tools", ""]);
    if no_session_persistence {
        cmd.arg("--no-session-persistence");
    }
    match system_prompt {
        SystemPrompt::Empty => {
            cmd.args(["--system-prompt", ""]);
        }
        SystemPrompt::Custom(s) => {
            cmd.args(["--system-prompt", s]);
        }
        SystemPrompt::ClaudeDefault => {}
    }
    if let Some(s) = system_prompt_append {
        cmd.args(["--append-system-prompt", s]);
    }
    // No --mcp-config: strict mode alone shuts out plugin-installed
    // servers and account-level claude.ai connectors.
    cmd.arg("--strict-mcp-config");
    if let Some(model) = &model {
        cmd.arg("--model").arg(model);
    }
    cmd.current_dir(&prep.cwd)
        .env("CLAUDE_CONFIG_DIR", &prep.claude_home)
        .env("CLAUDE_PROJECTS_DIR", &projects_dir)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .stdin(Stdio::null());
    let output = cmd.output();

    // Archive and clean up even when the spawn failed, so a partial
    // session is still preserved for inspection. With persistence off
    // there is no session to archive.
    let archived = if no_session_persistence {
        Ok(Vec::new())
    } else {
        archive_sessions(&prep.claude_home, &prep.archive_dir)
    };
    cleanup_run_dir(&prep.run_dir, &prep.cwd);
    let output = output?;
    archived?;
    Ok(output)
}

/// A spawned `claude -p --input-format stream-json --output-format
/// stream-json` child wired for event-by-event observation and live
/// stdin injection. `claude`'s output is parsed line-by-line into
/// [`StreamMessage`] values pushed onto an `mpsc` channel the caller
/// drains via [`recv_event`](Self::recv_event); stdin is held open
/// so the caller can `send_user_message` or `close_stdin` during the
/// session.
///
/// Lifecycle: drain events to the terminal `Result` message, then
/// `wait_exit` (reaps the child) and `finalize` (mirror stop + archive
/// sweep + run-dir cleanup). Dropping the session `SIGKILL`s the
/// child via `kill_on_drop`.
pub struct StreamingAgentSession {
    child: TokioChild,
    /// Held open until the caller invokes `close_stdin` or `stop`. `None`
    /// once closed; `send_user_message` returns `BrokenPipe` afterward.
    stdin: Option<ChildStdin>,
    events: tokio_mpsc::UnboundedReceiver<StreamMessage>,
    /// Background task that reads stdout line-by-line and parses each
    /// line into a [`StreamMessage`]. Joined during `finalize`.
    stdout_task: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    mirror: Option<SessionMirror>,
    finalized: bool,
    run_dir: PathBuf,
    cwd: PathBuf,
    claude_home: PathBuf,
    archive_dir: PathBuf,
    timeout: Option<usize>,
}

/// One parsed line from the `--output-format stream-json` stream. The
/// caller layer (`gage-runtime`) maps these to its Rune-visible `Event`
/// vocabulary; we keep the raw JSON here so all SDK fields stay
/// reachable without re-parsing.
#[derive(Debug, Clone)]
pub enum StreamMessage {
    /// `{"type": "system", ...}` — emitted once at session start with
    /// init info (session id, tools, mcp servers, model, cwd, …).
    System(serde_json::Value),
    /// `{"type": "assistant", "message": {...}, ...}` — one assistant
    /// turn carrying an Anthropic API message with content blocks
    /// (text, thinking, tool_use).
    Assistant(serde_json::Value),
    /// `{"type": "user", "message": {...}, ...}` — tool_result blocks
    /// produced by the harness in response to assistant tool_use.
    User(serde_json::Value),
    /// `{"type": "result", ...}` — terminal message carrying
    /// `is_error`, `stop_reason`, `duration_ms`, `cost_usd`, `usage`,
    /// `result` (final text), `session_id`, `uuid`, etc. No further
    /// messages follow.
    Result(serde_json::Value),
    /// Any `type` not enumerated above. Forward-compatible: SDK gains
    /// (`stream_event`, status, rate-limit, hook progress …) surface
    /// here unchanged.
    Other(serde_json::Value),
    /// A stdout line that did not parse as JSON. Carries the raw text
    /// and the parse error so diagnostic output isn't silently dropped.
    ParseError { line: String, error: String },
}

#[allow(clippy::too_many_arguments)]
async fn start_streaming_session_inner(
    prep: PreparedRun,
    model: Option<String>,
    max_turns: Option<u32>,
    timeout: Option<usize>,
    mcp_url: Option<String>,
    system_prompt: SystemPrompt,
    system_prompt_append: Option<String>,
    prompt: &str,
) -> io::Result<StreamingAgentSession> {
    let mut cmd = TokioCommand::new(&prep.claude_bin);
    // Headless mode without a prompt arg — the initial user message is
    // sent via the stream-json stdin channel below, matching how the
    // SDK drives `claude -p` with `--input-format stream-json`.
    cmd.arg("-p");
    cmd.args(["--input-format", "stream-json"]);
    cmd.args(["--output-format", "stream-json"]);
    cmd.args(["--tools", "WaitForMcpServers"]);
    // --print + --output-format=stream-json requires --verbose
    cmd.arg("--verbose");
    cmd.args(["--thinking-display", "summarized"]);
    cmd.arg("--disable-slash-commands");
    match &system_prompt {
        SystemPrompt::Empty => {
            cmd.args(["--system-prompt", ""]);
        }
        SystemPrompt::Custom(s) => {
            cmd.args(["--system-prompt", s]);
        }
        SystemPrompt::ClaudeDefault => {}
    }
    if let Some(s) = &system_prompt_append {
        cmd.args(["--append-system-prompt", s]);
    }
    if let Some(url) = &mcp_url {
        cmd.arg("--mcp-config").arg(mcp_config_json(url));
    }
    // Only the --mcp-config server (if any) — never plugin-installed
    // servers or account-level claude.ai connectors.
    cmd.arg("--strict-mcp-config");
    // `user` loads our seeded `settings.json` (permissions.allow),
    // which is required for headless tool calls to avoid prompts.
    // `project` / `local` are skipped so cwd settings don't leak in.
    cmd.arg("--setting-sources").arg("user");
    if let Some(model) = &model {
        cmd.arg("--model").arg(model);
    }
    if let Some(max_turns) = max_turns {
        cmd.arg("--max-turns").arg(max_turns.to_string());
    }
    let projects_dir = prep.claude_home.join("projects");
    cmd.current_dir(&prep.cwd)
        .env("CLAUDE_CONFIG_DIR", &prep.claude_home)
        .env("CLAUDE_PROJECTS_DIR", &projects_dir)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .env("ENABLE_TOOL_SEARCH", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    tracing::debug!(
        claude = %prep.claude_bin.display(),
        cwd = %prep.cwd.display(),
        mcp_url = ?mcp_url,
        model = ?model,
        max_turns = ?max_turns,
        "spawning streaming claude child",
    );
    let mut child = cmd.spawn()?;
    let pid = child.id();
    tracing::debug!(?pid, "streaming child spawned");
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = spawn_reader(child.stderr.take());

    // Send the initial user message before returning. Stays inside this
    // function so the caller observes the session in "running with one
    // turn queued" state; further turns go via
    // [`StreamingAgentSession::send_user_message`].
    if let Some(s) = stdin.as_mut() {
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        });
        let mut buf = serde_json::to_vec(&msg).map_err(io::Error::other)?;
        buf.push(b'\n');
        s.write_all(&buf).await?;
        s.flush().await?;
        tracing::debug!(bytes = buf.len(), "sent initial user message");
    }

    let (tx, rx) = tokio_mpsc::unbounded_channel::<StreamMessage>();
    let stdout_task = stdout.map(|out| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            let mut n = 0u64;
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        n += 1;
                        let msg = parse_stream_message(&line);
                        tracing::debug!(
                            seq = n,
                            kind = stream_message_kind(&msg),
                            line_len = line.len(),
                            line = %line,
                            "stream-json line",
                        );
                        if tx.send(msg).is_err() {
                            tracing::debug!(seq = n, "stream-json channel closed by receiver");
                            return;
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(total = n, "stream-json stdout EOF");
                        return;
                    }
                    Err(e) => {
                        eprintln!("warning: stream-json stdout read error: {e}");
                        tracing::warn!(error = %e, "stream-json stdout read error");
                        return;
                    }
                }
            }
        })
    });

    let mirror = match start_session_mirror(&projects_dir, &prep.archive_dir) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("warning: session mirror watcher failed to start: {e}");
            None
        }
    };

    Ok(StreamingAgentSession {
        child,
        stdin,
        events: rx,
        stdout_task,
        stderr,
        mirror,
        finalized: false,
        run_dir: prep.run_dir,
        cwd: prep.cwd,
        claude_home: prep.claude_home,
        archive_dir: prep.archive_dir,
        timeout,
    })
}

fn stream_message_kind(m: &StreamMessage) -> &'static str {
    match m {
        StreamMessage::System(_) => "system",
        StreamMessage::Assistant(_) => "assistant",
        StreamMessage::User(_) => "user",
        StreamMessage::Result(_) => "result",
        StreamMessage::Other(_) => "other",
        StreamMessage::ParseError { .. } => "parse_error",
    }
}

fn parse_stream_message(line: &str) -> StreamMessage {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return StreamMessage::ParseError {
                line: line.to_string(),
                error: e.to_string(),
            };
        }
    };
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match ty {
        "system" => StreamMessage::System(v),
        "assistant" => StreamMessage::Assistant(v),
        "user" => StreamMessage::User(v),
        "result" => StreamMessage::Result(v),
        _ => StreamMessage::Other(v),
    }
}

impl StreamingAgentSession {
    /// OS process id while the child is running; `None` once it has exited.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Block until the next [`StreamMessage`] arrives. `None` once the
    /// stdout reader has reached EOF and the channel is drained — at
    /// which point the child has either exited or is about to.
    pub async fn recv_event(&mut self) -> Option<StreamMessage> {
        self.events.recv().await
    }

    /// Non-blocking variant of [`recv_event`](Self::recv_event). Returns
    /// `None` when the queue is empty *or* the channel is closed; the
    /// caller distinguishes via [`child_status`](Self::child_status).
    pub fn try_recv_event(&mut self) -> Option<StreamMessage> {
        self.events.try_recv().ok()
    }

    /// Non-blocking child status. `Ok(None)` means still running;
    /// `Ok(Some(status))` means exited with the given status.
    pub fn child_status(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Send a single user message as a stream-json line to the child's
    /// stdin. Wire shape: `{"type": "user", "message": {"role": "user",
    /// "content": <text>}}`. Returns `BrokenPipe` if stdin was already
    /// closed via [`close_stdin`](Self::close_stdin) or `stop`.
    pub async fn send_user_message(&mut self, text: &str) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed"))?;
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        });
        let mut buf = serde_json::to_vec(&msg).map_err(io::Error::other)?;
        buf.push(b'\n');
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Close stdin (EOF). The child's stream-json input ends; whether
    /// claude exits on EOF alone is the open detail noted in the
    /// design doc. Idempotent.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Send a control-request `interrupt` line on stdin. Wire shape:
    /// `{"type":"control_request","request_id":"<id>","request":{"subtype":"interrupt"}}`.
    /// Returns `BrokenPipe` if stdin was already closed.
    pub async fn send_interrupt(&mut self) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed"))?;
        let request_id = format!("interrupt-{}", Uuid::new_v4());
        let msg = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" },
        });
        let mut buf = serde_json::to_vec(&msg).map_err(io::Error::other)?;
        buf.push(b'\n');
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Await child exit, bounded by the configured timeout (default
    /// [`DEFAULT_WAIT_TIMEOUT`]). On timeout the child is shut down
    /// gracefully ([`TIMEOUT_GRACE`] before `SIGKILL`) before the
    /// `TimedOut` error returns.
    pub async fn wait_exit(&mut self) -> io::Result<ExitStatus> {
        let timeout = self
            .timeout
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(DEFAULT_WAIT_TIMEOUT);
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                self.terminate(TIMEOUT_GRACE).await?;
                Err(io::Error::new(io::ErrorKind::TimedOut, "agent timeout"))
            }
        }
    }

    /// `SIGTERM`, wait up to `grace`, `SIGKILL` if still alive.
    pub async fn kill(&mut self, grace: Duration) -> io::Result<()> {
        self.terminate(grace).await
    }

    async fn terminate(&mut self, grace: Duration) -> io::Result<()> {
        if let Some(pid) = self.child.id() {
            // SAFETY: pid identifies a child process this struct owns
            // and has not yet reaped; SIGTERM is a valid signal number.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(s) => {
                s?;
            }
            Err(_) => self.child.kill().await?,
        }
        Ok(())
    }

    /// Stop the mirror, hardlink the final session JSONL into the
    /// archive, and remove the run dir. Idempotent. Call after the
    /// child has exited.
    pub fn finalize(&mut self) -> io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        self.finalized = true;
        self.stop_mirror();
        archive_sessions(&self.claude_home, &self.archive_dir)?;
        cleanup_run_dir(&self.run_dir, &self.cwd);
        Ok(())
    }

    /// Drain captured stderr. Returns an empty `Vec` if the reader was
    /// never started or has already been taken.
    pub async fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        join_reader(self.stderr.take()).await
    }

    /// Join the background stdout reader so the task exits cleanly. A
    /// panic in the reader is logged and treated as a normal join —
    /// finalize still runs. A no-op once joined.
    pub async fn join_stdout(&mut self) {
        if let Some(handle) = self.stdout_task.take()
            && let Err(e) = handle.await
        {
            eprintln!("warning: stream-json stdout reader join error: {e}");
        }
    }

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

/// Assemble the throwaway run dir, seed the isolated home, and install the
/// gage plugin. `archive_dir` is where session JSONLs get hardlinked.
fn prepare_run(archive_dir: PathBuf, tools: &[String]) -> io::Result<PreparedRun> {
    let run_id = Uuid::new_v4().to_string();
    let run_dir = tmp_run_dir(&run_id);
    let cwd = run_dir.join("cwd");
    let claude_home = run_dir.join("claude");

    let projects_dir = claude_home.join("projects");
    fs::create_dir_all(&claude_home)?;
    fs::create_dir_all(&cwd)?;
    fs::create_dir_all(&archive_dir)?;
    fs::create_dir_all(&projects_dir)?;
    seed_claude_home(&claude_home, &cwd, tools)?;

    let claude_bin = find_claude()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "`claude` binary not on PATH"))?;

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

/// Populate the isolated home with the minimum needed to skip onboarding
/// and present a familiar UI, without inheriting any setting that could
/// shift model behavior or what lands in the transcript. `tools` is the
/// MCP-tool allowlist verbatim — no entry is implicit.
///
/// Does not seed `skills/` into the home. The user-facing gage plugin
/// ships a `tools` skill whose description eagerly loads MCP tool FQNs
/// into the model's context, sparing it from guessing names or burning
/// `ToolSearch` calls — that hack is intentionally not replicated
/// here. Callers of `call_agent` and `gage agent` who want similar
/// guidance compose it explicitly via `.append_system_prompt(...)`
/// or the prompt itself.
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
    // Server name in `--mcp-config` is plain `gage`, yielding the
    // `mcp__gage__` FQN prefix (see `mcp_config_json`).
    let mut allow_set: Vec<String> = Vec::with_capacity(tools.len());
    for t in tools {
        let prefixed = format!("mcp__gage__{t}");
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

fn tmp_run_dir(run_id: &str) -> PathBuf {
    gage_home().join("tmp").join(run_id)
}

fn agent_archive_dir(name: &str) -> PathBuf {
    gage_home().join("claude").join(name)
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

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn tool_policy_empty_allow_yields_empty() {
        // No implicit baseline — empty allow means no tools exposed.
        assert!(ToolPolicy::tools(vec![], vec![]).unwrap().is_empty());
    }

    #[test]
    fn tool_policy_allow_lists_listed_only() {
        let got = ToolPolicy::tools(s(&["IssueWrite"]), vec![]).unwrap();
        assert_eq!(got, s(&["IssueWrite"]));
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
        let got = ToolPolicy::tools(s(&["*"]), s(&["IssueWrite"])).unwrap();
        assert_eq!(got.len(), TOOL_NAMES.len() - 1);
        assert!(!got.iter().any(|g| g == "IssueWrite"));
    }

    #[test]
    fn tool_policy_explicit_pair() {
        let got = ToolPolicy::tools(s(&["Query", "IssueUpdate"]), vec![]).unwrap();
        // Order follows TOOL_NAMES.
        assert_eq!(got, s(&["Query", "IssueUpdate"]));
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
}
