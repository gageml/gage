//! Running a Codex agent for [`CodexDriver`](crate::driver::CodexDriver).
//!
//! The agent is one `codex exec --json` child: one prompt, read from
//! stdin, and one turn. It runs in a throwaway cwd under
//! `<gage home>/tmp/<run id>/` with the user's Codex config left out
//! (`--ignore-user-config`, which drops their MCP servers, plugins,
//! and settings but keeps their sign-in) and project docs off. Its
//! sandbox is read-only and it never asks for approval, so the only
//! things it can do are call the MCP tools the run serves and, when
//! the spec enables the shell, read.
//!
//! Codex names the session, the thread id, in the first event it
//! prints, so [`start`] waits for that event before it returns: the
//! caller needs the id at once, to say where a live reader finds the
//! transcript. The transcript is the rollout Codex writes under the
//! user's own Codex home, stamped with a thread source that session
//! listings leave out, until the run is cleaned up.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gage_core::config::gage_home;
use gage_session::{
    AgentEvent, AgentSession, AgentSpec, Driver, DriverError, ModelSizes, NativeSession,
    SourceOptions, SystemPrompt, ToolUseEntries, TurnResult,
};
use serde_json::{Value as Json, json};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::driver::{CodexDriver, CodexSource};
use crate::harness::find_codex;
use crate::model::resolved_model;

/// How long `wait_exit` waits when the spec sets no timeout
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// The `SIGTERM` to `SIGKILL` grace when a wait times out
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// How long [`start`] waits for Codex to name the session
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// The thread source stamped on every agent run. A rollout that
/// carries it is not a session a person started, so session listings
/// leave it out.
pub(crate) const THREAD_SOURCE: &str = "gage-scan";

/// The MCP server name the child sees. Tool names reach the model as
/// `mcp__gage__<name>`.
const MCP_SERVER_NAME: &str = "gage";

/// The built-in tools a spec can enable, by the names Codex gives
/// them
const SHELL_TOOL: &str = "exec_command";
const WEB_SEARCH_TOOL: &str = "web_search";

/// `_meta` key under which Codex sends the id of the model's call
/// record that made an MCP request. The record is a `response_item`
/// of the rollout, and its id is that entry's uuid.
const ITEM_ID_KEY: &str = "itemId";

/// Spawn the child, resolving the spec's model under `models`, and
/// wait for it to name the session. Requires a tokio runtime: the
/// child is a tokio process.
pub(crate) fn start(spec: AgentSpec, models: &ModelSizes) -> Result<CodexAgent, DriverError> {
    let codex =
        find_codex().ok_or_else(|| DriverError::Other("`codex` binary not on PATH".to_string()))?;
    let model = resolved_model(spec.model.as_deref(), models);
    let run_dir = gage_home().join("tmp").join(Uuid::new_v4().to_string());
    let cwd = run_dir.join("cwd");
    fs::create_dir_all(&cwd)?;
    let args = match exec_args(&spec, &model, &cwd) {
        Ok(args) => args,
        Err(e) => {
            remove_dir(&run_dir);
            return Err(e);
        }
    };
    if spec.max_turns.is_some() {
        tracing::debug!("the codex driver runs one turn; max_turns is not applied");
    }
    let mut cmd = Command::new(&codex);
    cmd.args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tracing::debug!(
        codex = %codex.display(),
        cwd = %cwd.display(),
        model = %model,
        "spawning codex agent",
    );
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            remove_dir(&run_dir);
            return Err(DriverError::Io(e));
        }
    };
    tracing::debug!(pid = ?child.id(), "codex agent spawned");

    // The pipes are read and written on plain threads, in blocking
    // mode, so that waiting for the first event below needs nothing
    // from the async runtime.
    let stdin = child.stdin.take().and_then(|s| s.into_owned_fd().ok());
    let stdout = child.stdout.take().and_then(|s| s.into_owned_fd().ok());
    let stderr = child.stderr.take().and_then(|s| s.into_owned_fd().ok());
    let prompt = spec.prompt.clone();
    if let Some(fd) = stdin {
        thread::spawn(move || {
            // Dropping the file closes stdin, which Codex waits for
            if let Err(e) = File::from(fd).write_all(prompt.as_bytes()) {
                tracing::warn!(error = %e, "writing the prompt to codex");
            }
        });
    }
    let stderr = stderr.map(|fd| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            File::from(fd).read_to_end(&mut buf).map(|_| buf)
        })
    });
    let (tx, events) = mpsc::unbounded_channel();
    let (id_tx, id_rx) = std_mpsc::sync_channel(1);
    if let Some(fd) = stdout {
        thread::spawn(move || read_stream(File::from(fd), tx, id_tx));
    }

    let thread_id = match wait_for_thread_id(&id_rx) {
        Some(id) => id,
        None => {
            if let Err(e) = child.start_kill() {
                tracing::debug!(error = %e, "killing a codex agent that named no session");
            }
            let said = stderr
                .and_then(|handle| handle.join().ok())
                .and_then(Result::ok)
                .unwrap_or_default();
            remove_dir(&run_dir);
            return Err(DriverError::Other(no_session_error(
                &String::from_utf8_lossy(&said),
            )));
        }
    };
    Ok(CodexAgent {
        child,
        events,
        stderr,
        run_dir,
        timeout: spec.timeout,
        project: spec.project,
        buffer: VecDeque::new(),
        thread_id,
        model,
        started: Instant::now(),
        state: TurnState::default(),
        rollout: None,
    })
}

/// The arguments of the `codex exec` child for `spec`, run on `model`
/// in `cwd`. The prompt is not among them: it goes to stdin, named
/// here by the trailing `-`.
fn exec_args(spec: &AgentSpec, model: &str, cwd: &Path) -> Result<Vec<OsString>, DriverError> {
    let mut shell = false;
    let mut web_search = false;
    for tool in &spec.builtin_tools {
        match tool.as_str() {
            SHELL_TOOL => shell = true,
            WEB_SEARCH_TOOL => web_search = true,
            other => {
                return Err(DriverError::Other(format!(
                    "the codex driver has no built-in tool named {other:?}; \
                     it has {SHELL_TOOL} and {WEB_SEARCH_TOOL}"
                )));
            }
        }
    }
    let mut args: Vec<OsString> = Vec::new();
    let mut arg = |s: &str| args.push(OsString::from(s));
    arg("exec");
    arg("--json");
    arg("--skip-git-repo-check");
    arg("--ignore-user-config");
    arg("--sandbox");
    arg("read-only");
    arg("--thread-source");
    arg(THREAD_SOURCE);
    arg("--model");
    arg(model);
    arg("--cd");
    args.push(cwd.as_os_str().to_os_string());
    let mut config = |key: &str, value: Json| {
        args.push(OsString::from("-c"));
        // A JSON string or array of strings is the same text in TOML,
        // which is what Codex parses the value as
        args.push(OsString::from(format!("{key}={value}")));
    };
    config("project_doc_max_bytes", json!(0));
    config(
        "web_search",
        json!(if web_search { "live" } else { "disabled" }),
    );
    // The base instructions are Codex's own unless the spec replaces
    // them or asks for none. Appended text rides as developer
    // instructions, which Codex adds to whatever the base is; asking
    // for an append on an empty prompt keeps the base, as the spec
    // says an append implies the default.
    match &spec.system_prompt {
        SystemPrompt::Custom(text) => config("instructions", json!(text)),
        SystemPrompt::Empty if spec.system_prompt_append.is_none() => {
            config("instructions", json!(""));
        }
        SystemPrompt::Empty | SystemPrompt::Default => {}
    }
    if let Some(text) = &spec.system_prompt_append {
        config("developer_instructions", json!(text));
    }
    if let Some(mcp) = &spec.mcp {
        let key = |name: &str| format!("mcp_servers.{MCP_SERVER_NAME}.{name}");
        config(&key("url"), json!(mcp.url));
        // Without this, exec refuses every tool not marked read-only
        config(&key("default_tools_approval_mode"), json!("approve"));
        config(&key("enabled_tools"), json!(mcp.tool_names));
    }
    if !shell {
        args.push(OsString::from("--disable"));
        args.push(OsString::from("shell_tool"));
    }
    args.push(OsString::from("-"));
    Ok(args)
}

/// Read stdout to its end: every line goes on as an event, and the
/// first thread id seen is reported on `id_tx`. Dropping `id_tx`
/// without sending says the stream ended with no session named.
fn read_stream(
    out: File,
    tx: mpsc::UnboundedSender<StreamLine>,
    id_tx: std_mpsc::SyncSender<String>,
) {
    let mut id_tx = Some(id_tx);
    let mut n = 0u64;
    for line in BufReader::new(out).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(error = %e, "codex stdout read error");
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        n += 1;
        tracing::debug!(seq = n, line = %line, "codex event line");
        let parsed = parse_line(&line);
        if let StreamLine::Event { value, .. } = &parsed
            && let Some(id) = thread_started_id(value)
            && let Some(id_tx) = id_tx.take()
            && id_tx.send(id.to_string()).is_err()
        {
            return;
        }
        if tx.send(parsed).is_err() {
            return;
        }
    }
    tracing::debug!(total = n, "codex stdout EOF");
}

/// Wait for the reader to report the thread id. The wait blocks this
/// thread, so on a multi-thread runtime the runtime is told, and its
/// other work, the MCP host among it, carries on.
fn wait_for_thread_id(id_rx: &std_mpsc::Receiver<String>) -> Option<String> {
    let wait = || id_rx.recv_timeout(START_TIMEOUT).ok();
    let multi_thread = tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi_thread {
        tokio::task::block_in_place(wait)
    } else {
        wait()
    }
}

/// The error for a child that ended, or said nothing for
/// [`START_TIMEOUT`], before naming a session. `stderr` is what it
/// wrote there.
fn no_session_error(stderr: &str) -> String {
    let said = stderr.trim();
    if said.is_empty() {
        "codex exec started no session".to_string()
    } else {
        format!("codex exec started no session: {said}")
    }
}

/// One stdout line
#[derive(Debug, Clone)]
enum StreamLine {
    /// An event, with the line as Codex wrote it
    Event {
        value: Json,
        line: String,
    },
    ParseError {
        line: String,
        error: String,
    },
}

fn parse_line(line: &str) -> StreamLine {
    match serde_json::from_str(line) {
        Ok(value) => StreamLine::Event {
            value,
            line: line.to_string(),
        },
        Err(e) => StreamLine::ParseError {
            line: line.to_string(),
            error: e.to_string(),
        },
    }
}

/// The thread id of a `thread.started` event
fn thread_started_id(event: &Json) -> Option<&str> {
    (event.get("type")?.as_str()? == "thread.started")
        .then(|| event.get("thread_id")?.as_str())
        .flatten()
}

/// What the run has reported so far
#[derive(Debug, Default)]
struct TurnState {
    /// Token counts summed over the turns ended so far. Codex reports
    /// each turn's own counts.
    usage: Usage,
    turns: i64,
    /// The last message the agent wrote
    last_text: String,
    /// The items whose call has been reported, so one that is first
    /// seen complete still reports its call
    called: HashSet<String>,
    /// A turn has ended, or the stream has: nothing is in flight
    idle: bool,
}

/// Token counts, in Codex's names
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Usage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
}

impl Usage {
    fn add(&mut self, turn: &Json) {
        let count = |name: &str| turn.get(name).and_then(Json::as_u64).unwrap_or(0);
        self.input_tokens += count("input_tokens");
        self.cached_input_tokens += count("cached_input_tokens");
        self.cache_write_input_tokens += count("cache_write_input_tokens");
        self.output_tokens += count("output_tokens");
        self.reasoning_output_tokens += count("reasoning_output_tokens");
    }

    fn to_json(self) -> Json {
        json!({
            "input_tokens": self.input_tokens,
            "cached_input_tokens": self.cached_input_tokens,
            "cache_write_input_tokens": self.cache_write_input_tokens,
            "output_tokens": self.output_tokens,
            "reasoning_output_tokens": self.reasoning_output_tokens,
        })
    }
}

/// What a turn's end needs beside the state: the session's id and
/// model, and how long it has run
struct TurnContext<'a> {
    thread_id: &'a str,
    model: &'a str,
    elapsed: Duration,
}

/// Translate one stdout line into events on `buf`.
fn expand(
    line: StreamLine,
    state: &mut TurnState,
    ctx: &TurnContext,
    buf: &mut VecDeque<AgentEvent>,
) {
    let (event, raw) = match line {
        StreamLine::Event { value, line } => (value, line),
        StreamLine::ParseError { line, error } => {
            buf.push_back(AgentEvent::ParseError(format!("{error}: {line}")));
            return;
        }
    };
    match event.get("type").and_then(Json::as_str).unwrap_or("") {
        "thread.started" | "turn.started" | "error" => {
            buf.push_back(AgentEvent::System(event.to_string()));
        }
        "item.started" => {
            if let Some(item) = event.get("item")
                && let Some(call) = tool_use_of(item)
            {
                state.called.insert(str_field(item, "id"));
                buf.push_back(call);
            }
        }
        "item.completed" => match event.get("item") {
            Some(item) => expand_completed(item, state, buf),
            None => buf.push_back(AgentEvent::Other(event.to_string())),
        },
        "turn.completed" => {
            if let Some(usage) = event.get("usage") {
                state.usage.add(usage);
            }
            state.turns += 1;
            state.idle = true;
            let text = state.last_text.clone();
            buf.push_back(turn_end(raw, state, ctx, text, false));
        }
        "turn.failed" => {
            state.turns += 1;
            state.idle = true;
            let message = event
                .pointer("/error/message")
                .and_then(Json::as_str)
                .unwrap_or("the turn failed")
                .to_string();
            buf.push_back(turn_end(raw, state, ctx, message, true));
        }
        // An item still in progress says nothing its completion will
        // not
        "item.updated" => {}
        _ => buf.push_back(AgentEvent::Other(event.to_string())),
    }
}

/// The events of one finished item
fn expand_completed(item: &Json, state: &mut TurnState, buf: &mut VecDeque<AgentEvent>) {
    match item.get("type").and_then(Json::as_str).unwrap_or("") {
        "agent_message" => {
            let text = str_field(item, "text");
            state.last_text = text.clone();
            buf.push_back(AgentEvent::Assistant(text));
        }
        "reasoning" => buf.push_back(AgentEvent::Thinking(str_field(item, "text"))),
        "error" => buf.push_back(AgentEvent::System(item.to_string())),
        _ => match tool_use_of(item) {
            Some(call) => {
                let id = str_field(item, "id");
                if !state.called.remove(&id) {
                    buf.push_back(call);
                }
                buf.push_back(AgentEvent::ToolResult {
                    id,
                    output: tool_output_of(item),
                });
            }
            None => buf.push_back(AgentEvent::Other(item.to_string())),
        },
    }
}

/// The call an item records, when the item is a tool call. The names
/// are the ones the read driver gives the same calls in `tool_use`.
fn tool_use_of(item: &Json) -> Option<AgentEvent> {
    let (name, input) = match item.get("type")?.as_str()? {
        "mcp_tool_call" => (
            format!(
                "mcp__{}__{}",
                str_field(item, "server"),
                str_field(item, "tool")
            ),
            item.get("arguments").cloned().unwrap_or_else(|| json!({})),
        ),
        "command_execution" => (
            SHELL_TOOL.to_string(),
            json!({ "command": item.get("command") }),
        ),
        "file_change" => (
            "apply_patch".to_string(),
            json!({ "changes": item.get("changes") }),
        ),
        "web_search" => (
            WEB_SEARCH_TOOL.to_string(),
            json!({ "query": item.get("query") }),
        ),
        _ => return None,
    };
    Some(AgentEvent::ToolUse {
        name,
        input: input.to_string(),
    })
}

/// What a finished tool call returned, as text: an MCP call's error
/// or its text parts joined, a command's output, and for anything
/// else its status.
fn tool_output_of(item: &Json) -> String {
    if let Some(message) = item.pointer("/error/message").and_then(Json::as_str) {
        return message.to_string();
    }
    if let Some(parts) = item.pointer("/result/content").and_then(Json::as_array) {
        return parts
            .iter()
            .map(|part| match part.get("text").and_then(Json::as_str) {
                Some(text) => text.to_string(),
                None => part.to_string(),
            })
            .collect();
    }
    if let Some(output) = item.get("aggregated_output").and_then(Json::as_str) {
        return output.to_string();
    }
    str_field(item, "status")
}

/// The end of a turn, whose event line is `raw`. The counters are the
/// session's totals as of the turn.
fn turn_end(
    raw: String,
    state: &TurnState,
    ctx: &TurnContext,
    text: String,
    is_error: bool,
) -> AgentEvent {
    let usage = state.usage.to_json();
    let mut model_usage = serde_json::Map::new();
    model_usage.insert(ctx.model.to_string(), usage.clone());
    AgentEvent::TurnEnd {
        result: Box::new(TurnResult {
            text,
            is_error,
            stop_reason: if is_error { "error" } else { "end_turn" }.to_string(),
            turns: state.turns,
            duration_ms: ctx.elapsed.as_millis() as i64,
            usage: usage.to_string(),
            model_usage: Json::Object(model_usage).to_string(),
            session_id: ctx.thread_id.to_string(),
            raw,
            // Codex reports no cost, no API time, no denials, and no
            // id for the result
            ..TurnResult::default()
        }),
        idle: true,
    }
}

fn str_field(v: &Json, key: &str) -> String {
    v.get(key).and_then(Json::as_str).unwrap_or("").to_string()
}

fn remove_dir(dir: &Path) {
    if let Err(e) = fs::remove_dir_all(dir)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), error = %e, "removing an agent run directory");
    }
}

/// The transcript entry behind a tool call, read from the call
/// itself: Codex sends the id of the model's call record with every
/// MCP request, and that id is the entry's uuid.
struct CallRecords;

impl ToolUseEntries for CallRecords {
    fn tool_use_entry(&self, meta: &serde_json::Map<String, Json>) -> Option<String> {
        meta.get(ITEM_ID_KEY)?.as_str().map(String::from)
    }
}

/// A running `codex exec` child
pub struct CodexAgent {
    child: Child,
    events: mpsc::UnboundedReceiver<StreamLine>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    run_dir: PathBuf,
    timeout: Option<Duration>,
    project: Option<String>,
    /// Events expanded from one stdout line and not yet returned
    buffer: VecDeque<AgentEvent>,
    /// The session's id, as Codex named it in its first event
    thread_id: String,
    /// The model the child was started on
    model: String,
    started: Instant,
    state: TurnState,
    /// The rollout, once the transcript has been asked for and found
    rollout: Option<PathBuf>,
}

impl CodexAgent {
    async fn terminate(&mut self, grace: Duration) -> io::Result<()> {
        self.signal_term();
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => {
                status?;
            }
            Err(_) => self.child.kill().await?,
        }
        Ok(())
    }

    fn signal_term(&self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: pid is a child this struct owns and has not
            // reaped; SIGTERM is a valid signal
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
}

#[async_trait]
impl AgentSession for CodexAgent {
    fn native_id(&self) -> &str {
        &self.thread_id
    }

    async fn next_event(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(ev) = self.buffer.pop_front() {
                return Some(ev);
            }
            let Some(line) = self.events.recv().await else {
                self.state.idle = true;
                return None;
            };
            let ctx = TurnContext {
                thread_id: &self.thread_id,
                model: &self.model,
                elapsed: self.started.elapsed(),
            };
            expand(line, &mut self.state, &ctx, &mut self.buffer);
        }
    }

    /// A `codex exec` child takes one prompt.
    async fn send(&mut self, _text: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the codex driver runs one-shot agents: an agent takes its prompt and no later message",
        ))
    }

    /// End a turn in flight. The child has no channel for an
    /// interrupt, so it is asked to terminate; one whose turn has
    /// ended is exiting on its own and is left to.
    async fn interrupt(&mut self) -> io::Result<()> {
        if !self.state.idle {
            self.signal_term();
        }
        Ok(())
    }

    /// The child's input was closed when its prompt was written.
    fn close_input(&mut self) {}

    async fn wait_exit(&mut self) -> io::Result<i32> {
        let timeout = self.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => Ok(status?.code().unwrap_or(-1)),
            Err(_) => {
                self.terminate(TIMEOUT_GRACE).await?;
                Err(io::Error::new(io::ErrorKind::TimedOut, "agent timeout"))
            }
        }
    }

    async fn kill(&mut self, grace: Duration) -> io::Result<()> {
        self.terminate(grace).await
    }

    fn tool_use_entries(&self) -> Arc<dyn ToolUseEntries> {
        Arc::new(CallRecords)
    }

    async fn take_stderr(&mut self) -> io::Result<Vec<u8>> {
        match self.stderr.take() {
            Some(handle) => tokio::task::spawn_blocking(move || handle.join())
                .await
                .map_err(io::Error::other)?
                .map_err(|_panic| io::Error::other("the stderr reader panicked"))?,
            None => Ok(Vec::new()),
        }
    }

    /// The run's rollout, found by the thread id among the sessions of
    /// the user's Codex home, agent threads included.
    fn transcript(&mut self) -> Result<Option<Box<dyn NativeSession + Send>>, DriverError> {
        let mut options = SourceOptions::new("");
        options.show_agents(true);
        let source = CodexDriver::new().open_source(&options)?;
        let codex = source
            .as_any()
            .downcast_ref::<CodexSource>()
            .expect("the codex driver opens CodexSource handles");
        let Some(mut session) = codex.agent_session(&self.thread_id)? else {
            return Ok(None);
        };
        self.rollout = Some(session.path().to_path_buf());
        if let Some(project) = &self.project {
            session.set_project(project.clone());
        }
        Ok(Some(Box::new(session)))
    }

    /// Remove the run's directory and its rollout. Codex's own index
    /// of threads keeps its row for the run.
    fn cleanup(&mut self) -> Result<(), DriverError> {
        if let Some(rollout) = self.rollout.take()
            && let Err(e) = fs::remove_file(&rollout)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(DriverError::Io(e));
        }
        if let Err(e) = fs::remove_dir_all(&self.run_dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(DriverError::Io(e));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use gage_session::AgentMcp;

    use super::*;

    fn spec(prompt: &str) -> AgentSpec {
        AgentSpec {
            prompt: prompt.to_string(),
            model: None,
            system_prompt: SystemPrompt::Empty,
            system_prompt_append: None,
            max_turns: None,
            timeout: None,
            project: None,
            mcp: None,
            builtin_tools: Vec::new(),
        }
    }

    fn args_of(spec: &AgentSpec) -> Vec<String> {
        exec_args(spec, "gpt-6-luna", Path::new("/run/cwd"))
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    /// The value given for the config key `key`, if any
    fn config_of<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
        let prefix = format!("{key}=");
        args.windows(2)
            .find(|w| w[0] == "-c" && w[1].starts_with(&prefix))
            .map(|w| &w[1][prefix.len()..])
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn a_plain_run_is_isolated_read_only_and_reads_its_prompt_from_stdin() {
        let args = args_of(&spec("hello"));
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--ignore-user-config".to_string()));
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
        assert!(has_pair(&args, "--sandbox", "read-only"));
        assert!(has_pair(&args, "--thread-source", THREAD_SOURCE));
        assert!(has_pair(&args, "--model", "gpt-6-luna"));
        assert!(has_pair(&args, "--cd", "/run/cwd"));
        assert!(has_pair(&args, "--disable", "shell_tool"));
        assert_eq!(config_of(&args, "project_doc_max_bytes"), Some("0"));
        assert_eq!(config_of(&args, "web_search"), Some("\"disabled\""));
        assert_eq!(args.last().map(String::as_str), Some("-"));
        // The prompt is never an argument
        assert!(!args.contains(&"hello".to_string()));
        // No MCP server unless the spec serves one
        assert!(!args.iter().any(|a| a.starts_with("mcp_servers.")));
    }

    #[test]
    fn the_system_prompt_modes_map_onto_codex_instructions() {
        // Empty: no base instructions
        let args = args_of(&spec("p"));
        assert_eq!(config_of(&args, "instructions"), Some("\"\""));
        assert_eq!(config_of(&args, "developer_instructions"), None);

        // Default: Codex's own
        let mut default = spec("p");
        default.system_prompt = SystemPrompt::Default;
        let args = args_of(&default);
        assert_eq!(config_of(&args, "instructions"), None);

        // Custom: a replacement, quoted as a TOML string
        let mut custom = spec("p");
        custom.system_prompt = SystemPrompt::Custom("Say \"hi\".\nThen stop.".to_string());
        let args = args_of(&custom);
        assert_eq!(
            config_of(&args, "instructions"),
            Some(r#""Say \"hi\".\nThen stop.""#)
        );

        // An append keeps the base and rides as developer instructions
        let mut append = spec("p");
        append.system_prompt_append = Some("Be brief.".to_string());
        let args = args_of(&append);
        assert_eq!(config_of(&args, "instructions"), None);
        assert_eq!(
            config_of(&args, "developer_instructions"),
            Some("\"Be brief.\"")
        );
    }

    #[test]
    fn an_mcp_server_is_served_by_url_with_its_tools_approved() {
        let mut with_mcp = spec("p");
        with_mcp.mcp = Some(AgentMcp {
            url: "http://127.0.0.1:1234/abc/mcp".to_string(),
            tool_names: vec!["query".to_string(), "finding".to_string()],
        });
        let args = args_of(&with_mcp);
        assert_eq!(
            config_of(&args, "mcp_servers.gage.url"),
            Some("\"http://127.0.0.1:1234/abc/mcp\"")
        );
        assert_eq!(
            config_of(&args, "mcp_servers.gage.default_tools_approval_mode"),
            Some("\"approve\"")
        );
        assert_eq!(
            config_of(&args, "mcp_servers.gage.enabled_tools"),
            Some(r#"["query","finding"]"#)
        );
    }

    #[test]
    fn built_in_tools_are_the_shell_and_web_search_and_nothing_else() {
        let mut shell = spec("p");
        shell.builtin_tools = vec![SHELL_TOOL.to_string(), WEB_SEARCH_TOOL.to_string()];
        let args = args_of(&shell);
        assert!(!has_pair(&args, "--disable", "shell_tool"));
        assert_eq!(config_of(&args, "web_search"), Some("\"live\""));
        // The sandbox stays read-only whatever is enabled
        assert!(has_pair(&args, "--sandbox", "read-only"));

        let mut other = spec("p");
        other.builtin_tools = vec!["Agent".to_string()];
        let err = exec_args(&other, "m", Path::new("/c")).unwrap_err();
        assert!(err.to_string().contains("no built-in tool named \"Agent\""));
    }

    /// Run the captured stdout `lines` through the expansion.
    fn events_of(lines: &[&str]) -> (Vec<AgentEvent>, TurnState) {
        let mut state = TurnState::default();
        let mut buf = VecDeque::new();
        let ctx = TurnContext {
            thread_id: "00000000-0000-7000-8000-000000000001",
            model: "gpt-6-luna",
            elapsed: Duration::from_millis(1500),
        };
        for line in lines {
            expand(parse_line(line), &mut state, &ctx, &mut buf);
        }
        (buf.into_iter().collect(), state)
    }

    const THREAD_STARTED: &str =
        r#"{"type":"thread.started","thread_id":"00000000-0000-7000-8000-000000000001"}"#;
    const TURN_STARTED: &str = r#"{"type":"turn.started"}"#;
    const MCP_STARTED: &str = r#"{"type":"item.started","item":{"id":"item_0","type":"mcp_tool_call","server":"gage","tool":"query","arguments":{"sql":"select 1"},"result":null,"error":null,"status":"in_progress"}}"#;
    const MCP_COMPLETED: &str = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"gage","tool":"query","arguments":{"sql":"select 1"},"result":{"content":[{"type":"text","text":"answer: "},{"type":"text","text":"42"}],"structured_content":null},"error":null,"status":"completed"}}"#;
    const MCP_DENIED: &str = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"gage","tool":"query","arguments":{"sql":"select 1"},"result":null,"error":{"message":"MCP tool call requires approval, but approval policy is never"},"status":"failed"}}"#;
    const MESSAGE: &str = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"answer: 42"}}"#;
    const REASONING: &str =
        r#"{"type":"item.completed","item":{"id":"item_2","type":"reasoning","text":"Checking."}}"#;
    const TURN_COMPLETED: &str = r#"{"type":"turn.completed","usage":{"input_tokens":20976,"cached_input_tokens":13440,"cache_write_input_tokens":0,"output_tokens":67,"reasoning_output_tokens":5}}"#;

    #[test]
    fn the_thread_id_is_read_from_the_first_event() {
        let StreamLine::Event { value: event, .. } = parse_line(THREAD_STARTED) else {
            panic!("a JSON line parses");
        };
        assert_eq!(
            thread_started_id(&event),
            Some("00000000-0000-7000-8000-000000000001")
        );
        let StreamLine::Event { value: other, .. } = parse_line(TURN_STARTED) else {
            panic!("a JSON line parses");
        };
        assert_eq!(thread_started_id(&other), None);
    }

    #[test]
    fn a_run_expands_to_calls_results_text_and_one_turn_end() {
        let (events, state) = events_of(&[
            THREAD_STARTED,
            TURN_STARTED,
            MCP_STARTED,
            MCP_COMPLETED,
            REASONING,
            MESSAGE,
            TURN_COMPLETED,
        ]);
        assert!(matches!(events[0], AgentEvent::System(_)));
        assert!(matches!(events[1], AgentEvent::System(_)));
        assert_eq!(
            events[2],
            AgentEvent::ToolUse {
                name: "mcp__gage__query".to_string(),
                input: r#"{"sql":"select 1"}"#.to_string(),
            }
        );
        assert_eq!(
            events[3],
            AgentEvent::ToolResult {
                id: "item_0".to_string(),
                output: "answer: 42".to_string(),
            }
        );
        assert_eq!(events[4], AgentEvent::Thinking("Checking.".to_string()));
        assert_eq!(events[5], AgentEvent::Assistant("answer: 42".to_string()));
        let AgentEvent::TurnEnd { result, idle } = &events[6] else {
            panic!("the run ends with a turn end, not {:?}", events[6]);
        };
        assert!(idle);
        assert!(state.idle);
        assert_eq!(result.text, "answer: 42");
        assert!(!result.is_error);
        assert_eq!(result.stop_reason, "end_turn");
        assert_eq!(result.turns, 1);
        assert_eq!(result.duration_ms, 1500);
        assert_eq!(result.cost_usd, None);
        assert_eq!(result.session_id, "00000000-0000-7000-8000-000000000001");
        let usage: Json = serde_json::from_str(&result.usage).unwrap();
        assert_eq!(usage["input_tokens"], 20976);
        assert_eq!(usage["cached_input_tokens"], 13440);
        assert_eq!(usage["output_tokens"], 67);
        assert_eq!(usage["reasoning_output_tokens"], 5);
        let by_model: Json = serde_json::from_str(&result.model_usage).unwrap();
        assert_eq!(by_model["gpt-6-luna"]["output_tokens"], 67);
        assert_eq!(result.raw, TURN_COMPLETED);
        assert_eq!(events.len(), 7);
    }

    #[test]
    fn a_call_first_seen_complete_still_reports_its_call() {
        let (events, _) = events_of(&[MCP_COMPLETED]);
        assert!(
            matches!(&events[0], AgentEvent::ToolUse { name, .. } if name == "mcp__gage__query")
        );
        assert!(matches!(&events[1], AgentEvent::ToolResult { .. }));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_refused_call_reports_the_refusal_as_its_result() {
        let (events, _) = events_of(&[MCP_STARTED, MCP_DENIED]);
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                id: "item_0".to_string(),
                output: "MCP tool call requires approval, but approval policy is never".to_string(),
            }
        );
    }

    #[test]
    fn usage_is_the_session_total_over_its_turns() {
        let (events, _) = events_of(&[TURN_COMPLETED, TURN_COMPLETED]);
        let AgentEvent::TurnEnd { result, .. } = &events[1] else {
            panic!("a turn end");
        };
        assert_eq!(result.turns, 2);
        let usage: Json = serde_json::from_str(&result.usage).unwrap();
        assert_eq!(usage["input_tokens"], 41952);
        assert_eq!(usage["output_tokens"], 134);
    }

    #[test]
    fn a_failed_turn_ends_in_error_with_codex_s_message() {
        let (events, state) = events_of(&[
            THREAD_STARTED,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `gpt-9` not found."}}"#,
            TURN_STARTED,
            r#"{"type":"error","message":"The 'gpt-9' model is not supported when using Codex with a ChatGPT account."}"#,
            r#"{"type":"turn.failed","error":{"message":"The 'gpt-9' model is not supported when using Codex with a ChatGPT account."}}"#,
        ]);
        let AgentEvent::TurnEnd { result, idle } = events.last().unwrap() else {
            panic!("a failed turn is a turn end");
        };
        assert!(idle);
        assert!(state.idle);
        assert!(result.is_error);
        assert_eq!(result.stop_reason, "error");
        assert_eq!(
            result.text,
            "The 'gpt-9' model is not supported when using Codex with a ChatGPT account."
        );
        // The harness's own messages come through as system events
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::System(_)))
                .count(),
            4
        );
    }

    #[test]
    fn a_line_that_is_not_json_is_a_parse_error() {
        let (events, _) = events_of(&["Reading additional input from stdin..."]);
        assert!(
            matches!(&events[0], AgentEvent::ParseError(e) if e.contains("Reading additional"))
        );
    }

    #[test]
    fn shell_and_patch_items_are_named_as_the_read_driver_names_them() {
        let (events, _) = events_of(&[
            r#"{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"/bin/bash -lc 'ls'","aggregated_output":"a\nb\n","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"/home/u/proj/a.txt","kind":"update"}],"status":"completed"}}"#,
        ]);
        assert!(matches!(&events[0], AgentEvent::ToolUse { name, .. } if name == "exec_command"));
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                id: "item_0".to_string(),
                output: "a\nb\n".to_string(),
            }
        );
        assert!(matches!(&events[2], AgentEvent::ToolUse { name, .. } if name == "apply_patch"));
        assert_eq!(
            events[3],
            AgentEvent::ToolResult {
                id: "item_1".to_string(),
                output: "completed".to_string(),
            }
        );
    }

    #[test]
    fn a_tool_call_names_its_entry_by_the_item_id_codex_sends() {
        let meta = json!({
            "callId": "exec-00000000-0000-4000-8000-000000000002",
            "itemId": "ctc_0000000000000000",
            "progressToken": 1,
        });
        let Json::Object(meta) = meta else {
            unreachable!()
        };
        assert_eq!(
            CallRecords.tool_use_entry(&meta).as_deref(),
            Some("ctc_0000000000000000")
        );
        assert_eq!(CallRecords.tool_use_entry(&serde_json::Map::new()), None);
    }

    #[test]
    fn a_child_that_names_no_session_reports_what_it_said() {
        assert_eq!(no_session_error(""), "codex exec started no session");
        assert_eq!(
            no_session_error("Error loading config.toml: invalid transport\n"),
            "codex exec started no session: Error loading config.toml: invalid transport"
        );
    }
}
