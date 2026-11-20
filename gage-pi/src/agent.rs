//! Running a Pi agent for [`PiDriver`](crate::driver::PiDriver).
//!
//! The agent is one `pi --mode json` child: one prompt, read from
//! stdin, run until Pi has no more work for it. It runs in a
//! throwaway cwd under `<gage home>/tmp/<run id>/` with the user's
//! extensions, skills, prompt templates, themes, and context files
//! left out. The user's agent directory is left alone so the child
//! finds their credentials there; the trade is that the session lands
//! in the user's own sessions directory, under the directory Pi names
//! for the tmp cwd, until the run is cleaned up.
//!
//! The MCP server a run serves is registered in the cwd's own
//! `.pi/mcp.json`, which Pi reads for a project it is told to trust.
//! An entry there replaces the user's server of the same name, and
//! the file turns off every other server the user has registered, so
//! the child connects to the run's server alone. The tools declared
//! to the model are named one by one.
//!
//! A system prompt goes to Pi as a file in the run's directory. Pi
//! takes the value of its system prompt options as a path when a file
//! is there and as the text otherwise, so text given as an argument
//! could be read as a path. Pi always adds the working directory to
//! its system prompt and has no option for none, so a spec that asks
//! for no system prompt gets one that says nothing else.
//!
//! Pi has no sandbox and asks for no permission before a tool call.
//! An agent with no built-in tools can only call the tools the run
//! serves; one given `bash`, `write`, or `edit` acts as the user.
//!
//! The session's id is chosen here and passed to Pi, so it is known
//! before the child starts.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gage_core::config::gage_home;
use gage_plugin::MCP_SERVER_NAME;
use gage_session::{
    AgentEvent, AgentMcp, AgentSession, AgentSpec, Driver, DriverError, ModelSizes, NativeSession,
    SourceOptions, SystemPrompt, ToolUseEntries, TurnResult,
};
use serde_json::{Map, Value as Json, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::derive::{blocks, line_type, role, str_at};
use crate::driver::{PiDriver, PiSource};
use crate::harness::find_pi;
use crate::home::{Location, agent_dir};
use crate::model::resolved_model;
use crate::plugin::TOOL_PREFIX;
use crate::session_file::session_files;

/// How long `wait_exit` waits when the spec sets no timeout
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// The `SIGTERM` to `SIGKILL` grace when a wait times out
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// The built-in tools a spec can enable, by the names Pi gives them
const BUILTIN_TOOLS: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls"];

/// The directory of a project in which Pi reads the project's own
/// configuration
const PROJECT_CONFIG_DIR: &str = ".pi";

/// The file of MCP server registrations, in a project's configuration
/// directory and in the agent directory
const MCP_CONFIG: &str = "mcp.json";

/// The file under a run's directory that holds the system prompt
/// which replaces Pi's own
const SYSTEM_PROMPT_FILE: &str = "system-prompt.md";

/// The file under a run's directory that holds the text added to the
/// system prompt
const APPEND_PROMPT_FILE: &str = "append-system-prompt.md";

/// The system prompt that says nothing. Pi takes an empty one as none
/// given and uses its own.
const BLANK_PROMPT: &str = "\n";

/// Whether `cwd` is the working directory of one of Gage's own agent
/// runs. A run's cwd is `<gage home>/tmp/<run id>/cwd`, and a session
/// started there is the run's alone; general listings leave it out.
pub(crate) fn is_agent_cwd(cwd: &Path) -> bool {
    cwd.starts_with(gage_home().join("tmp"))
}

/// Spawn the child, resolving the spec's model under `models`.
/// Requires a tokio runtime: the child's pipes are tokio handles and
/// its readers are tokio tasks.
pub(crate) fn start(spec: AgentSpec, models: &ModelSizes) -> Result<PiAgent, DriverError> {
    let pi = find_pi().ok_or_else(|| DriverError::Other("`pi` binary not on PATH".to_string()))?;
    let model = resolved_model(spec.model.as_deref(), models);
    let session_id = Uuid::new_v4().to_string();
    let run_dir = gage_home().join("tmp").join(Uuid::new_v4().to_string());
    let args = agent_args(&spec, &model, &session_id, &run_dir)?;
    if spec.max_turns.is_some() {
        tracing::debug!("the pi driver runs an agent until it settles; max_turns is not applied");
    }
    let cwd = run_dir.join("cwd");
    let prepared = fs::create_dir_all(&cwd)
        .and_then(|()| write_prompts(&run_dir, &spec))
        .and_then(|()| match &spec.mcp {
            Some(mcp) => write_mcp_config(&cwd, mcp, &user_servers()),
            None => Ok(()),
        });
    if let Err(e) = prepared {
        remove_dir(&run_dir);
        return Err(DriverError::Io(e));
    }
    let mut cmd = Command::new(&pi);
    cmd.args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tracing::debug!(
        pi = %pi.display(),
        cwd = %cwd.display(),
        model = %model,
        session_id = %session_id,
        "spawning pi agent",
    );
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            remove_dir(&run_dir);
            return Err(DriverError::Io(e));
        }
    };
    tracing::debug!(pid = ?child.id(), "pi agent spawned");

    if let Some(mut stdin) = child.stdin.take() {
        let prompt = spec.prompt.clone();
        tokio::spawn(async move {
            // Dropping the handle closes stdin, which Pi reads to its
            // end for the prompt
            if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
                tracing::warn!(error = %e, "writing the prompt to pi");
            }
        });
    }
    let stderr = child.stderr.take().map(|mut err| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            err.read_to_end(&mut buf).await.map(|_| buf)
        })
    });
    let (tx, events) = mpsc::unbounded_channel();
    if let Some(out) = child.stdout.take() {
        tokio::spawn(read_stream(out, tx));
    }
    Ok(PiAgent {
        child,
        events,
        stderr,
        run_dir,
        timeout: spec.timeout,
        project: spec.project,
        buffer: VecDeque::new(),
        session_id,
        model,
        started: Instant::now(),
        state: RunState::default(),
        session_file: Arc::new(OnceLock::new()),
    })
}

/// The arguments of the `pi` child for `spec`, run on `model` as the
/// session `session_id`, with its system prompt files under
/// `run_dir`. The prompt is not among them: it goes to stdin.
fn agent_args(
    spec: &AgentSpec,
    model: &str,
    session_id: &str,
    run_dir: &Path,
) -> Result<Vec<OsString>, DriverError> {
    for tool in &spec.builtin_tools {
        if !BUILTIN_TOOLS.contains(&tool.as_str()) {
            return Err(DriverError::Other(format!(
                "the pi driver has no built-in tool named {tool:?}; it has {}",
                BUILTIN_TOOLS.join(", ")
            )));
        }
    }
    let mut args: Vec<OsString> = Vec::new();
    let mut arg = |s: &str| args.push(OsString::from(s));
    arg("--mode");
    arg("json");
    arg("--session-id");
    arg(session_id);
    arg("--model");
    arg(model);
    // The update check and the model catalog refresh Pi does at
    // startup are the user's own session's to do
    arg("--offline");
    arg("--no-extensions");
    arg("--no-skills");
    arg("--no-prompt-templates");
    arg("--no-themes");
    arg("--no-context-files");
    match &spec.mcp {
        Some(_) => {
            // MCP is one of Pi's built-in extensions, which
            // `--no-extensions` turns off with the rest
            arg("--extension");
            arg("builtin:mcp");
            // The run's own `.pi/mcp.json` is read only for a project
            // Pi is told to trust
            arg("--approve");
        }
        None => {
            arg("--no-mcp");
            arg("--no-approve");
        }
    }
    if replaced_prompt(spec).is_some() {
        args.push(OsString::from("--system-prompt"));
        args.push(run_dir.join(SYSTEM_PROMPT_FILE).into_os_string());
    }
    if spec.system_prompt_append.is_some() {
        args.push(OsString::from("--append-system-prompt"));
        args.push(run_dir.join(APPEND_PROMPT_FILE).into_os_string());
    }
    let tools: Vec<String> = spec
        .builtin_tools
        .iter()
        .cloned()
        .chain(
            spec.mcp
                .iter()
                .flat_map(|mcp| &mcp.tool_names)
                .map(|name| format!("{TOOL_PREFIX}{name}")),
        )
        .collect();
    if tools.is_empty() {
        args.push(OsString::from("--no-tools"));
    } else {
        // An allowlist that names an MCP tool is the whole selection:
        // no other tool, built in or served, is declared to the model
        args.push(OsString::from("--tools"));
        args.push(OsString::from(tools.join(",")));
    }
    Ok(args)
}

/// The text that replaces Pi's own system prompt for `spec`; `None`
/// when Pi's own stays. Pi's own stays unless the spec replaces it or
/// asks for none, and asking for an append on an empty prompt keeps
/// it, as the spec says an append implies the default.
fn replaced_prompt(spec: &AgentSpec) -> Option<&str> {
    match &spec.system_prompt {
        SystemPrompt::Custom(text) => Some(text),
        SystemPrompt::Empty if spec.system_prompt_append.is_none() => Some(BLANK_PROMPT),
        SystemPrompt::Empty | SystemPrompt::Default => None,
    }
}

/// Write the system prompt files of `spec` under `run_dir`.
fn write_prompts(run_dir: &Path, spec: &AgentSpec) -> io::Result<()> {
    if let Some(text) = replaced_prompt(spec) {
        fs::write(run_dir.join(SYSTEM_PROMPT_FILE), text)?;
    }
    if let Some(text) = &spec.system_prompt_append {
        fs::write(run_dir.join(APPEND_PROMPT_FILE), text)?;
    }
    Ok(())
}

/// The names of the MCP servers the user has registered with Pi. A
/// registration file that cannot be read names none: the run's own
/// server is still the only one whose tools are declared.
fn user_servers() -> Vec<String> {
    let Some(path) = agent_dir().map(|dir| dir.join(MCP_CONFIG)) else {
        return Vec::new();
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "reading the user's MCP servers");
            return Vec::new();
        }
    };
    match serde_json::from_slice::<Json>(&bytes) {
        Ok(config) => config
            .get("mcpServers")
            .and_then(Json::as_object)
            .map(|servers| servers.keys().cloned().collect())
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "reading the user's MCP servers");
            Vec::new()
        }
    }
}

/// Write the run's `.pi/mcp.json` under `cwd`: the server `mcp` names,
/// with its tools declared to the model, and each of `user_servers`
/// turned off. Pi takes two names that differ only in `-` and `_` as
/// one server, so a user's server so named is the run's and is not
/// turned off.
fn write_mcp_config(cwd: &Path, mcp: &AgentMcp, user_servers: &[String]) -> io::Result<()> {
    let same_server = |name: &str| name.replace('-', "_") == MCP_SERVER_NAME.replace('-', "_");
    let mut servers = Map::new();
    for name in user_servers.iter().filter(|name| !same_server(name)) {
        servers.insert(name.clone(), json!({ "enabled": false }));
    }
    servers.insert(
        MCP_SERVER_NAME.to_string(),
        json!({ "url": mcp.url, "exposure": "direct" }),
    );
    let dir = cwd.join(PROJECT_CONFIG_DIR);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join(MCP_CONFIG),
        serde_json::to_vec_pretty(&json!({ "mcpServers": servers })).map_err(io::Error::other)?,
    )
}

/// Read stdout to its end, sending every line on as it is read. Pi
/// ends a record with a line feed alone, which is the only break a
/// line is split at here.
async fn read_stream(out: tokio::process::ChildStdout, tx: mpsc::UnboundedSender<StreamLine>) {
    let mut lines = BufReader::new(out).lines();
    let mut n = 0u64;
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "pi stdout read error");
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        n += 1;
        tracing::debug!(seq = n, line = %line, "pi event line");
        if tx.send(parse_line(&line)).is_err() {
            return;
        }
    }
    tracing::debug!(total = n, "pi stdout EOF");
}

/// One stdout line
#[derive(Debug, Clone)]
enum StreamLine {
    /// An event, with the line as Pi wrote it
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

/// What the run has reported so far
#[derive(Debug, Default)]
struct RunState {
    /// Token counts and cost summed over the model's responses so
    /// far. Pi reports each response's own.
    usage: Usage,
    /// Responses of the model, each with the tool calls it made
    turns: i64,
    /// The time the provider took over the responses so far
    api_ms: i64,
    /// The last text the agent wrote
    last_text: String,
    /// How the model's last response ended, in Pi's word for it
    stop_reason: String,
    /// What Pi said went wrong with the last response, when it failed
    error: Option<String>,
    /// Pi has no more work for the agent, or the stream has ended:
    /// nothing is in flight
    idle: bool,
}

/// Token counts and cost, in Pi's names
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total_tokens: u64,
    /// Pi's estimate from the model's listed prices, in USD
    cost: f64,
}

impl Usage {
    fn add(&mut self, response: &Json) {
        let count = |name: &str| response.get(name).and_then(Json::as_u64).unwrap_or(0);
        self.input += count("input");
        self.output += count("output");
        self.cache_read += count("cacheRead");
        self.cache_write += count("cacheWrite");
        self.total_tokens += count("totalTokens");
        self.cost += response
            .pointer("/cost/total")
            .and_then(Json::as_f64)
            .unwrap_or(0.0);
    }

    fn to_json(self) -> Json {
        json!({
            "input": self.input,
            "output": self.output,
            "cacheRead": self.cache_read,
            "cacheWrite": self.cache_write,
            "totalTokens": self.total_tokens,
        })
    }
}

/// What a turn's end needs beside the state: the session's id and
/// model, and how long it has run
struct RunContext<'a> {
    session_id: &'a str,
    model: &'a str,
    elapsed: Duration,
}

/// Translate one stdout line into events on `buf`.
fn expand(
    line: StreamLine,
    state: &mut RunState,
    ctx: &RunContext,
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
        "session" => buf.push_back(AgentEvent::System(raw)),
        "message_end" => {
            if let Some(message) = event.get("message")
                && message.get("role").and_then(Json::as_str) == Some("assistant")
            {
                expand_response(message, state, buf);
            }
        }
        "tool_execution_end" => buf.push_back(AgentEvent::ToolResult {
            id: str_field(&event, "toolCallId"),
            output: tool_output_of(&event),
        }),
        "turn_end" => state.turns += 1,
        "agent_settled" => {
            state.idle = true;
            let aborted = event.get("aborted").and_then(Json::as_bool) == Some(true);
            buf.push_back(turn_end(raw, state, ctx, aborted));
        }
        // A message's start and its updates say nothing its end does
        // not, the end of the system, user, and tool result messages
        // says nothing the run was not given or has not reported, and
        // a tool's start and the run's own start and end say nothing
        // the messages and the settling do not.
        "message_start"
        | "message_update"
        | "tool_execution_start"
        | "tool_execution_update"
        | "agent_start"
        | "agent_end"
        | "turn_start" => {}
        _ => buf.push_back(AgentEvent::Other(raw)),
    }
}

/// The events of one response of the model: its text, its thinking,
/// and the tool calls it made, in the order it wrote them.
fn expand_response(message: &Json, state: &mut RunState, buf: &mut VecDeque<AgentEvent>) {
    if let Some(usage) = message.get("usage") {
        state.usage.add(usage);
    }
    state.api_ms += message
        .get("durationMs")
        .and_then(Json::as_i64)
        .unwrap_or(0);
    state.stop_reason = str_field(message, "stopReason");
    state.error = message
        .get("errorMessage")
        .and_then(Json::as_str)
        .map(String::from);
    let parts = message.get("content").and_then(Json::as_array);
    for part in parts.into_iter().flatten() {
        match part.get("type").and_then(Json::as_str).unwrap_or("") {
            "text" => {
                let text = str_field(part, "text");
                state.last_text = text.clone();
                buf.push_back(AgentEvent::Assistant(text));
            }
            "thinking" => buf.push_back(AgentEvent::Thinking(str_field(part, "thinking"))),
            "toolCall" => buf.push_back(AgentEvent::ToolUse {
                name: str_field(part, "name"),
                input: part
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
                    .to_string(),
            }),
            _ => buf.push_back(AgentEvent::Other(part.to_string())),
        }
    }
}

/// What a finished tool call returned, as text: the text parts of its
/// result joined, and any other part as Pi wrote it.
fn tool_output_of(event: &Json) -> String {
    let parts = event.pointer("/result/content").and_then(Json::as_array);
    parts
        .into_iter()
        .flatten()
        .map(|part| match part.get("text").and_then(Json::as_str) {
            Some(text) => text.to_string(),
            None => part.to_string(),
        })
        .collect()
}

/// The end of the run's work, whose event line is `raw`. The counters
/// are the session's totals. A response that failed or was cut off is
/// an error, with what Pi said of it as the text.
fn turn_end(raw: String, state: &RunState, ctx: &RunContext, aborted: bool) -> AgentEvent {
    let is_error = aborted || matches!(state.stop_reason.as_str(), "error" | "aborted");
    let text = match (&state.error, is_error) {
        (Some(error), true) => error.clone(),
        _ => state.last_text.clone(),
    };
    let usage = state.usage.to_json();
    let mut model_usage = Map::new();
    model_usage.insert(ctx.model.to_string(), usage.clone());
    AgentEvent::TurnEnd {
        result: Box::new(TurnResult {
            text,
            is_error,
            stop_reason: stop_reason_of(&state.stop_reason, aborted).to_string(),
            turns: state.turns,
            duration_ms: ctx.elapsed.as_millis() as i64,
            duration_api_ms: state.api_ms,
            cost_usd: Some(state.usage.cost),
            usage: usage.to_string(),
            model_usage: Json::Object(model_usage).to_string(),
            session_id: ctx.session_id.to_string(),
            raw,
            // Pi reports no denials and no id for the result
            ..TurnResult::default()
        }),
        idle: true,
    }
}

/// Pi's word for how a response ended, in the words Gage records. Pi
/// has one word for each provider's own.
fn stop_reason_of(pi: &str, aborted: bool) -> &str {
    match pi {
        _ if aborted => "aborted",
        "stop" => "end_turn",
        "length" => "max_tokens",
        "toolUse" => "tool_use",
        other => other,
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

/// The session file of the session `session_id` under the default
/// location, once Pi has written it. Pi names the file for the time
/// the session started, so it is found by the id its name carries.
fn find_session_file(session_id: &str) -> Option<PathBuf> {
    let location = Location::default_location()?;
    let files = match session_files(&location.sessions_dir) {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(error = %e, "looking for an agent's session file");
            return None;
        }
    };
    files
        .into_iter()
        .find(|file| file.id == session_id)
        .map(|file| file.path)
}

/// The session entry behind a tool call, read from the session file.
/// Pi sends nothing with an MCP request that names the call, so the
/// entry is the last response in the file that calls one of the run's
/// tools. Pi writes a response to the file before it runs the
/// response's tool calls, and runs the calls of one response before
/// the model writes the next, so that response is the one the request
/// came from.
struct LastToolCall {
    session_id: String,
    /// The session file, once Pi has written it and it has been found
    session_file: Arc<OnceLock<PathBuf>>,
}

impl ToolUseEntries for LastToolCall {
    fn tool_use_entry(&self, _meta: &Map<String, Json>) -> Option<String> {
        let path = match self.session_file.get() {
            Some(path) => path,
            None => {
                let found = find_session_file(&self.session_id)?;
                self.session_file.get_or_init(|| found)
            }
        };
        match fs::read(path) {
            Ok(bytes) => last_tool_call_entry(&bytes),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "reading an agent's session file");
                None
            }
        }
    }
}

/// The id of the last entry of the session file `bytes` that is a
/// response calling one of the run's MCP tools.
fn last_tool_call_entry(bytes: &[u8]) -> Option<String> {
    let calls_served_tool = |line: &Json| {
        line_type(line) == Some("message")
            && role(line) == Some("assistant")
            && blocks(line.pointer("/message/content"))
                .iter()
                .any(|block| {
                    block.get("type").and_then(Json::as_str) == Some("toolCall")
                        && str_at(block, "name").is_some_and(|name| name.starts_with(TOOL_PREFIX))
                })
    };
    bytes
        .split(|b| *b == b'\n')
        .rev()
        // A line that is not JSON is one Pi skips; the last line can
        // be one Pi is still writing
        .filter_map(|line| serde_json::from_slice::<Json>(line).ok())
        .find(calls_served_tool)
        .and_then(|line| str_at(&line, "id").map(String::from))
}

/// A running `pi --mode json` child
pub struct PiAgent {
    child: Child,
    events: mpsc::UnboundedReceiver<StreamLine>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    run_dir: PathBuf,
    timeout: Option<Duration>,
    project: Option<String>,
    /// Events expanded from one stdout line and not yet returned
    buffer: VecDeque<AgentEvent>,
    /// The session's id, chosen before the child started
    session_id: String,
    /// The model the child was started on
    model: String,
    started: Instant,
    state: RunState,
    /// The session file, once Pi has written it and it has been
    /// found. The index of tool calls shares it.
    session_file: Arc<OnceLock<PathBuf>>,
}

impl PiAgent {
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
impl AgentSession for PiAgent {
    fn native_id(&self) -> &str {
        &self.session_id
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
            let ctx = RunContext {
                session_id: &self.session_id,
                model: &self.model,
                elapsed: self.started.elapsed(),
            };
            expand(line, &mut self.state, &ctx, &mut self.buffer);
        }
    }

    /// A `pi --mode json` child takes one prompt.
    async fn send(&mut self, _text: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the pi driver runs one-shot agents: an agent takes its prompt and no later message",
        ))
    }

    /// End work in flight. The child has no channel for an interrupt,
    /// so it is asked to terminate; one that has settled is exiting on
    /// its own and is left to.
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
        Arc::new(LastToolCall {
            session_id: self.session_id.clone(),
            session_file: Arc::clone(&self.session_file),
        })
    }

    async fn take_stderr(&mut self) -> io::Result<Vec<u8>> {
        match self.stderr.take() {
            Some(handle) => handle.await.map_err(io::Error::other)?,
            None => Ok(Vec::new()),
        }
    }

    /// The run's session, found by its id among the sessions of the
    /// user's Pi agent directory, agent sessions included.
    fn transcript(&mut self) -> Result<Option<Box<dyn NativeSession + Send>>, DriverError> {
        let mut options = SourceOptions::new("");
        options.show_agents(true);
        let source = PiDriver::new().open_source(&options)?;
        let pi = source
            .as_any()
            .downcast_ref::<PiSource>()
            .expect("the pi driver opens PiSource handles");
        let Some(mut session) = pi.agent_session(&self.session_id)? else {
            return Ok(None);
        };
        // The file is the same one whichever way it was found
        self.session_file
            .get_or_init(|| session.path().to_path_buf());
        if let Some(project) = &self.project {
            session.set_project(project.clone());
        }
        Ok(Some(Box::new(session)))
    }

    /// Remove the run's directory, its session file, and the
    /// directory Pi made for the run's sessions, which held that one
    /// file.
    fn cleanup(&mut self) -> Result<(), DriverError> {
        let session_file = self
            .session_file
            .get()
            .cloned()
            .or_else(|| find_session_file(&self.session_id));
        if let Some(file) = session_file {
            remove_session_file(&file)?;
        }
        if let Err(e) = fs::remove_dir_all(&self.run_dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(DriverError::Io(e));
        }
        Ok(())
    }
}

/// Remove the session file `file` and the directory it is in. The
/// directory is the one Pi names for the run's cwd, which no other
/// session shares; one that still holds something is left.
fn remove_session_file(file: &Path) -> Result<(), DriverError> {
    if let Err(e) = fs::remove_file(file)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(DriverError::Io(e));
    }
    let Some(dir) = file.parent() else {
        return Ok(());
    };
    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(DriverError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
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

    fn mcp() -> AgentMcp {
        AgentMcp {
            url: "http://127.0.0.1:4000/abc".to_string(),
            tool_names: vec!["note_write".to_string(), "query".to_string()],
        }
    }

    fn args_of(spec: &AgentSpec) -> Vec<String> {
        agent_args(spec, "anthropic/claude-haiku-4-5", "s-1", Path::new("/run"))
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].as_str())
    }

    #[test]
    fn a_plain_run_has_no_tools_and_none_of_the_users_setup() {
        let args = args_of(&spec("hello"));
        assert_eq!(value_of(&args, "--mode"), Some("json"));
        assert_eq!(value_of(&args, "--session-id"), Some("s-1"));
        assert_eq!(
            value_of(&args, "--model"),
            Some("anthropic/claude-haiku-4-5")
        );
        for flag in [
            "--offline",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-mcp",
            "--no-approve",
            "--no-tools",
        ] {
            assert!(has(&args, flag), "{flag}: {args:?}");
        }
        assert!(!has(&args, "--approve"));
        assert!(!has(&args, "--tools"));
        // The prompt is never an argument
        assert!(!has(&args, "hello"));
    }

    #[test]
    fn a_run_with_tools_declares_exactly_the_ones_it_names() {
        let mut spec = spec("hello");
        spec.mcp = Some(mcp());
        spec.builtin_tools = vec!["read".to_string()];
        let args = args_of(&spec);
        assert_eq!(
            value_of(&args, "--tools"),
            Some("read,mcp__gage__note_write,mcp__gage__query")
        );
        assert_eq!(value_of(&args, "--extension"), Some("builtin:mcp"));
        assert!(has(&args, "--approve"));
        assert!(!has(&args, "--no-mcp"));
        assert!(!has(&args, "--no-tools"));

        spec.builtin_tools = vec!["Bash".to_string()];
        let err = agent_args(&spec, "m", "s-1", Path::new("/run"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no built-in tool named \"Bash\""), "{err}");
    }

    /// The system prompt files a run of `spec` is given, by option:
    /// the text of each, read back from where the arguments point
    fn prompts_of(spec: &AgentSpec) -> (Option<String>, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        write_prompts(dir.path(), spec).unwrap();
        let args: Vec<String> = agent_args(spec, "m", "s-1", dir.path())
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let text_of =
            |flag: &str| value_of(&args, flag).map(|path| fs::read_to_string(path).unwrap());
        (
            text_of("--system-prompt"),
            text_of("--append-system-prompt"),
        )
    }

    #[test]
    fn the_system_prompt_is_pis_own_unless_replaced_or_emptied() {
        // Pi takes an empty prompt as none given, so none is a blank
        assert_eq!(prompts_of(&spec("p")), (Some("\n".to_string()), None));

        let mut custom = spec("p");
        custom.system_prompt = SystemPrompt::Custom("Be brief.".to_string());
        assert_eq!(prompts_of(&custom), (Some("Be brief.".to_string()), None));

        let mut default = spec("p");
        default.system_prompt = SystemPrompt::Default;
        assert_eq!(prompts_of(&default), (None, None));

        // An append implies Pi's own prompt, even over an empty one
        let mut appended = spec("p");
        appended.system_prompt_append = Some("Also this.".to_string());
        assert_eq!(
            prompts_of(&appended),
            (None, Some("Also this.".to_string()))
        );
    }

    /// A prompt is never an argument: Pi reads an argument that names
    /// a file as that file.
    #[test]
    fn a_system_prompt_is_passed_as_a_file() {
        let mut custom = spec("p");
        custom.system_prompt = SystemPrompt::Custom("/etc/hostname".to_string());
        let args = args_of(&custom);
        assert_eq!(
            value_of(&args, "--system-prompt"),
            Some("/run/system-prompt.md")
        );
        assert!(!has(&args, "/etc/hostname"));
    }

    #[test]
    fn the_runs_server_replaces_the_users_and_the_others_are_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let users = ["gage".to_string(), "docs".to_string(), "Gage".to_string()];
        write_mcp_config(dir.path(), &mcp(), &users).unwrap();
        let text = fs::read_to_string(dir.path().join(".pi").join("mcp.json")).unwrap();
        let config: Json = serde_json::from_str(&text).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(
            servers["gage"],
            json!({"url": "http://127.0.0.1:4000/abc", "exposure": "direct"})
        );
        assert_eq!(servers["docs"], json!({"enabled": false}));
        assert_eq!(servers["Gage"], json!({"enabled": false}));
        assert_eq!(servers.len(), 3);
    }

    fn expanded(lines: &[&str]) -> (Vec<AgentEvent>, RunState) {
        let mut state = RunState::default();
        let mut buf = VecDeque::new();
        let ctx = RunContext {
            session_id: "s-1",
            model: "anthropic/claude-haiku-4-5",
            elapsed: Duration::from_millis(1500),
        };
        for line in lines {
            expand(parse_line(line), &mut state, &ctx, &mut buf);
        }
        (buf.into_iter().collect(), state)
    }

    const CALL: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Run it."},{"type":"toolCall","id":"toolu_1","name":"mcp__gage__query","arguments":{"sql":"select 1"}}],"usage":{"input":100,"output":10,"cacheRead":5,"cacheWrite":7,"totalTokens":122,"cost":{"total":0.004}},"stopReason":"toolUse","durationMs":900}}"#;
    const RESULT: &str = r#"{"type":"tool_execution_end","toolCallId":"toolu_1","toolName":"mcp__gage__query","result":{"content":[{"type":"text","text":"one: 1"},{"type":"image","data":"x"}]},"isError":false}"#;
    const ANSWER: &str = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"It is 1."}],"usage":{"input":120,"output":6,"cacheRead":0,"cacheWrite":0,"totalTokens":126,"cost":{"total":0.002}},"stopReason":"stop","durationMs":500}}"#;
    const SETTLED: &str = r#"{"type":"agent_settled","aborted":false}"#;

    #[test]
    fn a_run_is_its_responses_its_tool_results_and_one_end() {
        let (events, _) = expanded(&[
            r#"{"type":"session","version":3,"id":"s-1","cwd":"/run/cwd"}"#,
            r#"{"type":"agent_start"}"#,
            r#"{"type":"turn_start"}"#,
            r#"{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"x"}}"#,
            CALL,
            r#"{"type":"tool_execution_start","toolCallId":"toolu_1","toolName":"mcp__gage__query","args":{}}"#,
            RESULT,
            r#"{"type":"turn_end","message":{},"toolResults":[]}"#,
            ANSWER,
            r#"{"type":"turn_end","message":{},"toolResults":[]}"#,
            r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
            SETTLED,
        ]);
        assert!(matches!(&events[0], AgentEvent::System(raw) if raw.contains("\"session\"")));
        assert_eq!(events[1], AgentEvent::Thinking("Run it.".to_string()));
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
                id: "toolu_1".to_string(),
                output: r#"one: 1{"data":"x","type":"image"}"#.to_string(),
            }
        );
        assert_eq!(events[4], AgentEvent::Assistant("It is 1.".to_string()));
        let AgentEvent::TurnEnd { result, idle } = &events[5] else {
            panic!("{:?}", events[5]);
        };
        assert!(idle);
        assert_eq!(events.len(), 6);
        assert_eq!(result.text, "It is 1.");
        assert!(!result.is_error);
        assert_eq!(result.stop_reason, "end_turn");
        assert_eq!(result.turns, 2);
        assert_eq!(result.duration_ms, 1500);
        assert_eq!(result.duration_api_ms, 1400);
        assert_eq!(result.session_id, "s-1");
        assert_eq!(result.raw, SETTLED);
        assert!((result.cost_usd.unwrap() - 0.006).abs() < 1e-9);
        let usage: Json = serde_json::from_str(&result.usage).unwrap();
        assert_eq!(
            usage,
            json!({"input": 220, "output": 16, "cacheRead": 5, "cacheWrite": 7, "totalTokens": 248})
        );
        let by_model: Json = serde_json::from_str(&result.model_usage).unwrap();
        assert_eq!(by_model["anthropic/claude-haiku-4-5"], usage);
    }

    #[test]
    fn a_failed_response_ends_the_run_as_an_error_with_what_pi_said() {
        let (events, state) = expanded(&[
            r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401 invalid key"}}"#,
            SETTLED,
        ]);
        assert!(state.idle);
        let AgentEvent::TurnEnd { result, .. } = &events[0] else {
            panic!("{:?}", events[0]);
        };
        assert!(result.is_error);
        assert_eq!(result.text, "401 invalid key");
        assert_eq!(result.stop_reason, "error");

        let (events, _) = expanded(&[ANSWER, r#"{"type":"agent_settled","aborted":true}"#]);
        let AgentEvent::TurnEnd { result, .. } = &events[1] else {
            panic!("{:?}", events[1]);
        };
        assert!(result.is_error);
        assert_eq!(result.stop_reason, "aborted");
    }

    #[test]
    fn an_unknown_event_and_an_unparsable_line_are_passed_on() {
        let (events, _) = expanded(&[r#"{"type":"auto_retry_start","attempt":1}"#, "not json"]);
        assert!(matches!(&events[0], AgentEvent::Other(raw) if raw.contains("auto_retry_start")));
        assert!(matches!(&events[1], AgentEvent::ParseError(e) if e.contains("not json")));
    }

    #[test]
    fn the_calling_entry_is_the_last_response_that_calls_a_served_tool() {
        let file = [
            r#"{"type":"session","version":3,"id":"s-1","cwd":"/run/cwd"}"#,
            r#"{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"mcp__gage__query","arguments":{}}]}}"#,
            r#"{"type":"message","id":"r1","message":{"role":"toolResult","toolCallId":"t1","content":[]}}"#,
            r#"{"type":"message","id":"a2","message":{"role":"assistant","content":[{"type":"text","text":"x"},{"type":"toolCall","id":"t2","name":"mcp__gage__note_write","arguments":{}}]}}"#,
            r#"{"type":"message","id":"a3","message":{"role":"assistant","content":[{"type":"toolCall","id":"t3","name":"read","arguments":{}}]}}"#,
            r#"{"type":"message","id":"a4","mess"#,
        ]
        .join("\n");
        assert_eq!(last_tool_call_entry(file.as_bytes()).as_deref(), Some("a2"));
        assert_eq!(last_tool_call_entry(b""), None);
    }

    #[test]
    fn a_session_file_is_removed_with_the_directory_it_was_alone_in() {
        let dir = tempfile::tempdir().unwrap();
        let alone = dir.path().join("--run--");
        fs::create_dir_all(&alone).unwrap();
        let file = alone.join("2026-01-01T00-00-00-000Z_s-1.jsonl");
        fs::write(&file, "{}").unwrap();
        remove_session_file(&file).unwrap();
        assert!(!alone.exists());
        // Gone already is not an error
        remove_session_file(&file).unwrap();

        let shared = dir.path().join("--shared--");
        fs::create_dir_all(&shared).unwrap();
        let file = shared.join("2026-01-01T00-00-00-000Z_s-2.jsonl");
        fs::write(&file, "{}").unwrap();
        fs::write(shared.join("other.jsonl"), "{}").unwrap();
        remove_session_file(&file).unwrap();
        assert!(!file.exists());
        assert!(shared.join("other.jsonl").exists());
    }

    #[test]
    fn an_agent_cwd_is_one_under_gages_tmp_directory() {
        let tmp = gage_home().join("tmp");
        assert!(is_agent_cwd(&tmp.join("run-1").join("cwd")));
        assert!(!is_agent_cwd(&gage_home().join("store.git")));
        assert!(!is_agent_cwd(Path::new("/home/u/project")));
    }
}
