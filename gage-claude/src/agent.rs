//! Running a Claude Code agent for [`ClaudeDriver`](crate::driver::ClaudeDriver).
//!
//! The agent is a `claude -p` child with stream-json on stdin and
//! stdout. It runs in a throwaway cwd under `<gage home>/tmp/<run id>/`
//! with the user's config sources quarantined: `--setting-sources ""`
//! drops user, project, and local settings (hooks, plugins,
//! permissions), `--strict-mcp-config` drops user MCP servers, and
//! `--settings` supplies one seeded settings file. `CLAUDE_CONFIG_DIR`
//! is left alone so the child finds the user's credentials where
//! Claude Code keeps them; the trade is that the transcript lands in
//! the user's own projects directory, under a slug that names the tmp
//! cwd, until the run is cleaned up. The initial prompt is written to
//! stdin on the first read, send, or interrupt, so the child is
//! observed in a "running with one turn queued" state from the start.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use gage_core::config::gage_home;
use gage_session::{
    AgentEvent, AgentOutcome, AgentSession, AgentSpec, Driver, DriverError, NativeSession,
    SystemPrompt,
};
use serde_json::Value as Json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::driver::{ClaudeDriver, ClaudeNativeSession, ClaudeSource};
use crate::model::resolved_model;
use crate::session::{encode_project_dir, projects_dir};

/// How long `wait_exit` waits when the spec sets no timeout
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(900);

/// The `SIGTERM` to `SIGKILL` grace when a wait times out
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// Spawn the child. Requires a tokio runtime: the child's pipes are
/// tokio handles and its stdout reader is a tokio task.
pub(crate) fn start(spec: AgentSpec) -> Result<ClaudeAgent, DriverError> {
    let allowed_tools: Vec<String> = spec
        .mcp
        .iter()
        .flat_map(|m| m.tool_names.iter())
        .map(|name| format!("{MCP_TOOL_PREFIX}{name}"))
        .collect();
    let run = prepare_run(&allowed_tools)?;
    let mut cmd = Command::new(&run.claude_bin);
    cmd.arg("-p");
    cmd.args(["--input-format", "stream-json"]);
    cmd.args(["--output-format", "stream-json"]);
    cmd.args(["--tools", "WaitForMcpServers"]);
    // --print with stream-json output requires --verbose
    cmd.arg("--verbose");
    cmd.args(["--thinking-display", "summarized"]);
    match &spec.system_prompt {
        SystemPrompt::Empty if spec.system_prompt_append.is_none() => {
            cmd.args(["--system-prompt", ""]);
        }
        SystemPrompt::Custom(s) => {
            cmd.args(["--system-prompt", s]);
        }
        SystemPrompt::Empty | SystemPrompt::Default => {}
    }
    if let Some(s) = &spec.system_prompt_append {
        cmd.args(["--append-system-prompt", s]);
    }
    cmd.args(isolation_args(&run.claude_home));
    if let Some(mcp) = &spec.mcp {
        cmd.arg("--mcp-config").arg(mcp_config_json(&mcp.url));
    }
    cmd.arg("--model")
        .arg(resolved_model(spec.model.as_deref()));
    if let Some(n) = spec.max_turns {
        cmd.arg("--max-turns").arg(n.to_string());
    }
    let live_projects = live_projects_dir(&run.cwd);
    cmd.current_dir(&run.cwd)
        .env("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1")
        .env("ENABLE_TOOL_SEARCH", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tracing::debug!(
        claude = %run.claude_bin.display(),
        cwd = %run.cwd.display(),
        model = ?spec.model,
        max_turns = ?spec.max_turns,
        "spawning claude agent",
    );
    let mut child = cmd.spawn()?;
    tracing::debug!(pid = ?child.id(), "claude agent spawned");
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = spawn_reader(child.stderr.take());
    let (tx, events) = mpsc::unbounded_channel();
    let stdout_task = stdout.map(|out| tokio::spawn(read_stream(out, tx)));
    Ok(ClaudeAgent {
        child,
        stdin,
        events,
        stdout_task,
        stderr,
        run_dir: run.run_dir,
        live_projects,
        timeout: spec.timeout,
        pending_prompt: Some(spec.prompt),
        project: spec.project,
        buffer: VecDeque::new(),
        session_id: None,
        pending_tasks: HashSet::new(),
    })
}

/// Parse each stdout line and send it on; stop at EOF or when the
/// receiver is gone.
async fn read_stream(out: tokio::process::ChildStdout, tx: mpsc::UnboundedSender<StreamMessage>) {
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
                tracing::debug!(seq = n, kind = msg.kind(), line = %line, "stream-json line");
                if tx.send(msg).is_err() {
                    tracing::debug!(seq = n, "stream-json receiver gone");
                    return;
                }
            }
            Ok(None) => {
                tracing::debug!(total = n, "stream-json stdout EOF");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "stream-json stdout read error");
                return;
            }
        }
    }
}

/// Read a piped child stream to its end on a background task
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

/// The MCP server name the child sees. Tool names reach the model as
/// `mcp__gage__<name>`, and the allowlist uses the same prefix. The
/// name must not start with `plugin_`: claude treats such a server as
/// plugin-installed and attaches plugin-identity context to it.
const MCP_SERVER_NAME: &str = "gage";

const MCP_TOOL_PREFIX: &str = "mcp__gage__";

/// The `--mcp-config` argument: one streamable-HTTP server at `url`
fn mcp_config_json(url: &str) -> String {
    serde_json::json!({
        "mcpServers": { MCP_SERVER_NAME: { "type": "http", "url": url } }
    })
    .to_string()
}

/// `--strict-mcp-config` limits MCP to the `--mcp-config` server, when
/// there is one; `--setting-sources ""` suppresses user, project, and
/// local settings; `--settings` supplies the seeded file.
fn isolation_args(claude_home: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--strict-mcp-config"),
        OsString::from("--setting-sources"),
        OsString::new(),
        OsString::from("--settings"),
        claude_home.join("settings.json").into_os_string(),
    ]
}

/// Where the child writes this run's transcript: the user's projects
/// directory under the slug of the run's cwd. The slug contains
/// `--gage-tmp-`, which session listings filter out.
fn live_projects_dir(cwd: &Path) -> PathBuf {
    let root = projects_dir().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME should be set")
            .join(".claude")
            .join("projects")
    });
    root.join(encode_project_dir(cwd))
}

struct PreparedRun {
    run_dir: PathBuf,
    cwd: PathBuf,
    claude_home: PathBuf,
    claude_bin: PathBuf,
}

/// The throwaway run directory: an empty cwd for the child and the
/// seeded settings file. `allowed_tools` are the full tool names the
/// child runs without prompting.
fn prepare_run(allowed_tools: &[String]) -> Result<PreparedRun, DriverError> {
    let run_dir = gage_home().join("tmp").join(Uuid::new_v4().to_string());
    let cwd = run_dir.join("cwd");
    let claude_home = run_dir.join("claude");
    fs::create_dir_all(&cwd)?;
    fs::create_dir_all(&claude_home)?;
    seed_settings(&claude_home, allowed_tools)?;
    let claude_bin = find_claude()
        .ok_or_else(|| DriverError::Other("`claude` binary not on PATH".to_string()))?;
    Ok(PreparedRun {
        run_dir,
        cwd,
        claude_home,
        claude_bin,
    })
}

/// The one settings file the child reads: thinking summaries on, the
/// user's theme and tui settings, and the permission allowlist naming
/// the MCP tools served to this run.
fn seed_settings(claude_home: &Path, allowed_tools: &[String]) -> io::Result<()> {
    let user_settings = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| read_json(&home.join(".claude").join("settings.json")));
    let mut settings = serde_json::Map::new();
    settings.insert("showThinkingSummaries".into(), Json::Bool(true));
    for key in ["theme", "tui"] {
        if let Some(v) = user_settings.as_ref().and_then(|s| s.get(key)) {
            settings.insert(key.into(), v.clone());
        }
    }
    let allow = allowed_tools
        .iter()
        .map(|t| Json::String(t.clone()))
        .collect();
    let mut permissions = serde_json::Map::new();
    permissions.insert("allow".into(), Json::Array(allow));
    settings.insert("permissions".into(), Json::Object(permissions));
    fs::write(
        claude_home.join("settings.json"),
        serde_json::to_vec_pretty(&Json::Object(settings)).map_err(io::Error::other)?,
    )
}

fn read_json(path: &Path) -> Option<Json> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn find_claude() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("claude"))
        .find(|candidate| candidate.is_file())
}

/// One parsed stdout line
#[derive(Debug, Clone)]
enum StreamMessage {
    System(Json),
    Assistant(Json),
    User(Json),
    Result(Json),
    Other(Json),
    ParseError { line: String, error: String },
}

impl StreamMessage {
    fn kind(&self) -> &'static str {
        match self {
            StreamMessage::System(_) => "system",
            StreamMessage::Assistant(_) => "assistant",
            StreamMessage::User(_) => "user",
            StreamMessage::Result(_) => "result",
            StreamMessage::Other(_) => "other",
            StreamMessage::ParseError { .. } => "parse_error",
        }
    }
}

fn parse_stream_message(line: &str) -> StreamMessage {
    let v: Json = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return StreamMessage::ParseError {
                line: line.to_string(),
                error: e.to_string(),
            };
        }
    };
    match v.get("type").and_then(Json::as_str).unwrap_or("") {
        "system" => StreamMessage::System(v),
        "assistant" => StreamMessage::Assistant(v),
        "user" => StreamMessage::User(v),
        "result" => StreamMessage::Result(v),
        _ => StreamMessage::Other(v),
    }
}

/// A running `claude -p` child
pub struct ClaudeAgent {
    child: Child,
    /// `None` once closed
    stdin: Option<ChildStdin>,
    events: mpsc::UnboundedReceiver<StreamMessage>,
    stdout_task: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
    run_dir: PathBuf,
    live_projects: PathBuf,
    timeout: Option<Duration>,
    /// The initial user message, until it is written
    pending_prompt: Option<String>,
    project: Option<String>,
    /// Events expanded from one stream message and not yet returned
    buffer: VecDeque<AgentEvent>,
    /// From the `system/init` message
    session_id: Option<String>,
    /// Background sub-agents the child reports as started and not
    /// yet finished. Claude Code resumes the parent turn as each one
    /// finishes, so a turn end with tasks pending is not the session's
    /// end.
    pending_tasks: HashSet<String>,
}

impl ClaudeAgent {
    async fn flush_prompt(&mut self) -> io::Result<()> {
        if let Some(prompt) = self.pending_prompt.take() {
            self.write_user_message(&prompt).await?;
        }
        Ok(())
    }

    async fn write_user_message(&mut self, text: &str) -> io::Result<()> {
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        });
        self.write_line(&msg).await
    }

    async fn write_line(&mut self, msg: &Json) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed"))?;
        let mut buf = serde_json::to_vec(msg).map_err(io::Error::other)?;
        buf.push(b'\n');
        stdin.write_all(&buf).await?;
        stdin.flush().await
    }

    /// Translate one stream message into events on the buffer
    fn expand(&mut self, msg: StreamMessage) {
        match msg {
            StreamMessage::System(v) => {
                if let Some(subtype) = v.get("subtype").and_then(Json::as_str)
                    && let Some(task_id) = v.get("task_id").and_then(Json::as_str)
                {
                    match subtype {
                        "task_started" => {
                            self.pending_tasks.insert(task_id.to_string());
                        }
                        "task_notification" => {
                            self.pending_tasks.remove(task_id);
                        }
                        _ => {}
                    }
                }
                if self.session_id.is_none()
                    && v.get("subtype").and_then(Json::as_str) == Some("init")
                    && let Some(sid) = v.get("session_id").and_then(Json::as_str)
                {
                    self.session_id = Some(sid.to_string());
                }
                self.buffer.push_back(AgentEvent::System(v.to_string()));
            }
            StreamMessage::Assistant(v) => expand_assistant(&v, &mut self.buffer),
            StreamMessage::User(v) => expand_user(&v, &mut self.buffer),
            StreamMessage::Result(v) => {
                let outcome = outcome_of(&v);
                if self.session_id.is_none() && !outcome.session_id.is_empty() {
                    self.session_id = Some(outcome.session_id.clone());
                }
                self.buffer.push_back(AgentEvent::TurnEnd {
                    outcome: Box::new(outcome),
                    idle: self.pending_tasks.is_empty(),
                });
            }
            StreamMessage::Other(v) => self.buffer.push_back(AgentEvent::Other(v.to_string())),
            StreamMessage::ParseError { line, error } => self
                .buffer
                .push_back(AgentEvent::ParseError(format!("{error}: {line}"))),
        }
    }

    async fn terminate(&mut self, grace: Duration) -> io::Result<()> {
        if let Some(pid) = self.child.id() {
            // SAFETY: pid is a child this struct owns and has not
            // reaped; SIGTERM is a valid signal
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
        Ok(())
    }

    /// The transcript file: the one named by the session id, else the
    /// only `.jsonl` in the run's slot
    fn transcript_path(&self) -> io::Result<Option<PathBuf>> {
        if let Some(sid) = &self.session_id {
            let path = self.live_projects.join(format!("{sid}.jsonl"));
            return Ok(path.is_file().then_some(path));
        }
        let entries = match fs::read_dir(&self.live_projects) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut found = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                found.push(path);
            }
        }
        match found.as_slice() {
            [one] => Ok(Some(one.clone())),
            _ => Ok(None),
        }
    }
}

#[async_trait]
impl AgentSession for ClaudeAgent {
    async fn next_event(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(ev) = self.buffer.pop_front() {
                return Some(ev);
            }
            if let Err(e) = self.flush_prompt().await {
                tracing::warn!(error = %e, "writing the initial prompt");
            }
            let msg = self.events.recv().await?;
            self.expand(msg);
        }
    }

    async fn send(&mut self, text: &str) -> io::Result<()> {
        self.flush_prompt().await?;
        self.write_user_message(text).await
    }

    async fn interrupt(&mut self) -> io::Result<()> {
        self.flush_prompt().await?;
        let msg = serde_json::json!({
            "type": "control_request",
            "request_id": format!("interrupt-{}", Uuid::new_v4()),
            "request": { "subtype": "interrupt" },
        });
        self.write_line(&msg).await
    }

    fn close_input(&mut self) {
        self.pending_prompt = None;
        self.stdin = None;
    }

    async fn wait_exit(&mut self) -> io::Result<i32> {
        let timeout = self.timeout.unwrap_or(DEFAULT_WAIT_TIMEOUT);
        let status = match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                self.terminate(TIMEOUT_GRACE).await?;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "agent timeout"));
            }
        };
        if let Some(task) = self.stdout_task.take()
            && let Err(e) = task.await
        {
            tracing::warn!(error = %e, "stream-json reader join");
        }
        Ok(status.code().unwrap_or(-1))
    }

    async fn kill(&mut self, grace: Duration) -> io::Result<()> {
        self.terminate(grace).await
    }

    async fn take_stderr(&mut self) -> io::Result<Vec<u8>> {
        match self.stderr.take() {
            Some(handle) => handle.await.map_err(io::Error::other)?,
            None => Ok(Vec::new()),
        }
    }

    fn transcript(&mut self) -> Result<Option<Box<dyn NativeSession + Send>>, DriverError> {
        let Some(path) = self.transcript_path()? else {
            return Ok(None);
        };
        let native_id = match &self.session_id {
            Some(sid) => sid.clone(),
            None => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let meta = fs::metadata(&path)?;
        // The default source's index store caches the summary, as it
        // does for every session in the user's projects directory
        let source = ClaudeDriver::new().open_source("")?;
        let claude = source
            .as_any()
            .downcast_ref::<ClaudeSource>()
            .expect("the claude driver opens ClaudeSource handles");
        let mut session = ClaudeNativeSession::open(
            &native_id,
            &path,
            meta.modified()?,
            meta.len(),
            claude.root(),
            &claude.index_store(),
        )?;
        if let Some(project) = &self.project {
            session.set_project(project.clone());
        }
        Ok(Some(Box::new(session)))
    }

    fn cleanup(&mut self) -> Result<(), DriverError> {
        for dir in [&self.live_projects, &self.run_dir] {
            if let Err(e) = fs::remove_dir_all(dir)
                && e.kind() != io::ErrorKind::NotFound
            {
                return Err(DriverError::Io(e));
            }
        }
        Ok(())
    }
}

fn expand_assistant(v: &Json, buf: &mut VecDeque<AgentEvent>) {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Json::as_array)
    else {
        buf.push_back(AgentEvent::Other(v.to_string()));
        return;
    };
    for block in content {
        match block.get("type").and_then(Json::as_str).unwrap_or("") {
            "text" => buf.push_back(AgentEvent::Assistant(str_field(block, "text"))),
            "thinking" => buf.push_back(AgentEvent::Thinking(str_field(block, "thinking"))),
            "tool_use" => buf.push_back(AgentEvent::ToolUse {
                name: str_field(block, "name"),
                input: block
                    .get("input")
                    .map(Json::to_string)
                    .unwrap_or_else(|| "{}".to_string()),
            }),
            _ => buf.push_back(AgentEvent::Other(block.to_string())),
        }
    }
}

fn expand_user(v: &Json, buf: &mut VecDeque<AgentEvent>) {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Json::as_array)
    else {
        buf.push_back(AgentEvent::Other(v.to_string()));
        return;
    };
    for block in content {
        if block.get("type").and_then(Json::as_str) == Some("tool_result") {
            buf.push_back(AgentEvent::ToolResult {
                id: str_field(block, "tool_use_id"),
                output: tool_result_output(block),
            });
        } else {
            buf.push_back(AgentEvent::Other(block.to_string()));
        }
    }
}

/// The text of a tool_result block: a string, or the text parts of an
/// array joined; anything else as JSON
fn tool_result_output(block: &Json) -> String {
    let Some(content) = block.get("content") else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(parts) = content.as_array() {
        return parts
            .iter()
            .map(|part| match part.get("text").and_then(Json::as_str) {
                Some(text) => text.to_string(),
                None => part.to_string(),
            })
            .collect();
    }
    content.to_string()
}

/// The outcome carried by a `result` message. Absent fields are
/// empty or zero.
fn outcome_of(v: &Json) -> AgentOutcome {
    let json_field = |k: &str| v.get(k).map(Json::to_string).unwrap_or_default();
    AgentOutcome {
        text: str_field(v, "result"),
        is_error: v.get("is_error").and_then(Json::as_bool).unwrap_or(false),
        stop_reason: v
            .get("stop_reason")
            .and_then(Json::as_str)
            .unwrap_or("end_turn")
            .to_string(),
        turns: int_field(v, "num_turns"),
        duration_ms: int_field(v, "duration_ms"),
        duration_api_ms: int_field(v, "duration_api_ms"),
        cost_usd: v
            .get("total_cost_usd")
            .and_then(Json::as_f64)
            .unwrap_or(0.0),
        usage: json_field("usage"),
        model_usage: json_field("modelUsage"),
        permission_denials: json_field("permission_denials"),
        structured_output: json_field("structured_output"),
        session_id: str_field(v, "session_id"),
        uuid: str_field(v, "uuid"),
        raw: v.to_string(),
    }
}

fn str_field(v: &Json, key: &str) -> String {
    v.get(key).and_then(Json::as_str).unwrap_or("").to_string()
}

fn int_field(v: &Json, key: &str) -> i64 {
    v.get(key).and_then(Json::as_i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_blocks_expand_one_event_each() {
        let v: Json = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "thinking", "thinking": "hm" },
                { "type": "text", "text": "hi" },
                { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
            ]}
        });
        let mut buf = VecDeque::new();
        expand_assistant(&v, &mut buf);
        assert_eq!(
            Vec::from(buf),
            [
                AgentEvent::Thinking("hm".into()),
                AgentEvent::Assistant("hi".into()),
                AgentEvent::ToolUse {
                    name: "Bash".into(),
                    input: r#"{"command":"ls"}"#.into()
                },
            ]
        );
    }

    #[test]
    fn tool_results_join_their_text_parts() {
        let v: Json = serde_json::json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": "t1",
                  "content": [ { "type": "text", "text": "a" }, { "type": "text", "text": "b" } ] },
            ]}
        });
        let mut buf = VecDeque::new();
        expand_user(&v, &mut buf);
        assert_eq!(
            Vec::from(buf),
            [AgentEvent::ToolResult {
                id: "t1".into(),
                output: "ab".into()
            }]
        );
    }

    #[test]
    fn result_message_fills_the_outcome() {
        let v: Json = serde_json::json!({
            "type": "result", "result": "done", "is_error": false, "stop_reason": "end_turn",
            "num_turns": 3, "duration_ms": 1200, "duration_api_ms": 900, "total_cost_usd": 0.02,
            "usage": { "input_tokens": 5 }, "session_id": "S", "uuid": "U",
        });
        let o = outcome_of(&v);
        assert_eq!(o.text, "done");
        assert_eq!(o.turns, 3);
        assert_eq!(o.cost_usd, 0.02);
        assert_eq!(o.usage, r#"{"input_tokens":5}"#);
        assert_eq!(o.session_id, "S");
        assert!(o.raw.contains("\"uuid\":\"U\""));
        let missing = outcome_of(&serde_json::json!({ "type": "result" }));
        assert_eq!(missing.stop_reason, "end_turn");
        assert_eq!(missing.text, "");
    }

    #[test]
    fn unparseable_lines_are_parse_errors() {
        assert!(matches!(
            parse_stream_message("not json"),
            StreamMessage::ParseError { .. }
        ));
        assert!(matches!(
            parse_stream_message(r#"{"type":"result"}"#),
            StreamMessage::Result(_)
        ));
    }
}
