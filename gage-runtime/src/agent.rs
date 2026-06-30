//! `call_agent(prompt)` — Rune builder that, on await, spawns an
//! isolated `claude -p` child driven by an in-process MCP service
//! exposing a per-call tool set, and returns an [`Agent`] handle.
//!
//! This module owns the Rune-visible surface and the parsing of
//! builder arguments. The actual MCP service construction lives in
//! `gage-mcp` ([`gage_mcp::build_mcp_service`]); the claude child
//! spawn + event stream is the runtime's responsibility (steps 6.4/
//! 6.5 of the unify-llm-api work — currently stubbed).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;

use gage_agent::{
    AgentBuilder as GageAgentBuilder, SandboxSpec, StreamMessage, StreamingAgentSession,
};
use gage_mcp::{CustomToolCallback, ServiceHandle, ToolSpec};
use rune::Any;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, FromValue, Mut, Object, Protocol, Value, VmError};
use rune::{ContextError, Module};
use serde_json::Value as JsonValue;

use crate::error::Error;
use crate::state::current_scan_ctx;

/// Builder produced by `call_agent(prompt)`. Accumulates per-call
/// configuration; `await` validates and turns it into an [`Agent`].
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CallAgent {
    #[rune(skip)]
    prompt: String,
    #[rune(skip)]
    model: Option<String>,
    #[rune(skip)]
    max_turns: Option<u32>,
    #[rune(skip)]
    timeout_secs: Option<u64>,
    #[rune(skip)]
    system_prompt: Option<String>,
    #[rune(skip)]
    append_system_prompt: Option<String>,
    #[rune(skip)]
    name: Option<String>,
    /// Deferred parse outcome for `.gage_tools(...)`. Stored as a
    /// `Result` so we can surface argument-shape errors at `await`
    /// time rather than build time.
    #[rune(skip)]
    gage_tools: Option<super::Result<GageTools>>,
    #[rune(skip)]
    custom_tools: Option<super::Result<Vec<CustomToolDef>>>,
}

/// Resolved spec ready to feed into [`gage_mcp::ToolSpec`].
#[allow(dead_code)] // Fields consumed in 6.4 (claude spawn + MCP service registration).
pub(crate) struct CallSpec {
    pub prompt: String,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub gage_tools: GageTools,
    pub custom_tools: Vec<CustomToolDef>,
    /// Sandbox name passed to [`GageAgentBuilder::name`]. `None`
    /// leaves the builder default (`"default"`).
    pub name: Option<String>,
}

/// What the spec selects from the built-in Gage tool set.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Variant data consumed in 6.4.
pub(crate) enum GageTools {
    /// Caller omitted `.gage_tools(...)` entirely.
    None,
    /// Explicit list of tool names.
    Some(Vec<String>),
    /// `["*"]` — every built-in tool the host exposes.
    All,
}

/// One scanner-defined MCP tool. Mirrors the shape `SCANNER.tasks`
/// uses: keyed by Rune function name, the inner record carries the
/// wire-visible name, description, and input declaration.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Fields consumed in 6.4 (MCP tool route construction).
pub(crate) struct CustomToolDef {
    /// Rune function name that backs this tool (the dispatch key).
    pub fn_name: String,
    /// MCP tool name shown to the model.
    pub mcp_name: String,
    pub description: String,
    /// `inputs` shape from `SCANNER.tasks`. Parsed into a list so the
    /// caller can render it into a JSON schema later.
    pub inputs: Vec<InputDecl>,
}

/// One declared input on a scanner tool. Rendered into the MCP JSON
/// schema for the tool in 6.4.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Fields consumed in 6.4 (JSON schema rendering).
pub(crate) struct InputDecl {
    pub name: String,
    /// JSON Schema `type` (e.g. `"string"`, `"integer"`). Defaults to
    /// `"string"` when omitted.
    pub type_str: String,
    /// Defaults to `true` — every declared input is required unless
    /// `required: false` is set explicitly. Matches `call_llm`'s
    /// implicit semantics so the migration path is uniform.
    pub required: bool,
    pub description: Option<String>,
}

/// Handle to a running per-call agent. When the call declared any
/// tools (`.gage_tools(...)` or `.tools(...)`), an MCP service is
/// registered at `await` time and lives for the lifetime of this
/// value ([`ServiceHandle`] drops on `Drop`, unregistering from the
/// host). A call with no tools declared skips MCP entirely — the
/// agent runs pure text-in/text-out.
///
/// State is held under a single sync `Mutex<AgentInner>` that callers
/// briefly lock to inspect or `.take()` the streaming session; async
/// work happens with the session held in a local, not under the lock.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Agent {
    #[rune(skip)]
    spec: Arc<CallSpec>,
    /// MCP URL the claude child connects to. `None` when no tools
    /// were declared.
    #[rune(skip)]
    mcp_url: Option<String>,
    /// Per-call MCP service registration. Drop unregisters. `None`
    /// when no tools were declared (no MCP server is running).
    #[rune(skip)]
    _service: Option<ServiceHandle>,
    /// Permit from the run-wide `agent_pool` semaphore. Held for the
    /// lifetime of this `Agent` (acquired in `do_call_agent` before
    /// spawning the claude child); drop releases the slot for the next
    /// queued `call_agent` invocation.
    #[rune(skip)]
    _permit: tokio::sync::OwnedSemaphorePermit,
    #[rune(skip)]
    inner: Arc<Mutex<AgentInner>>,
}

/// Mutable agent state. `poll`/`wait` briefly lock to take the session
/// out for async work, then re-lock to push expanded events / record
/// the terminal result.
struct AgentInner {
    session: Option<StreamingAgentSession>,
    /// Events expanded from received [`StreamMessage`]s, ready for
    /// `poll()` to return one at a time.
    event_buf: VecDeque<Event>,
    /// `true` once `do_stop` has finalized the session and pushed the
    /// trailing [`Event::Stop`]. `running()` reports false and
    /// `result()` returns `Some` from this point on. Distinct from
    /// "saw a `Result` message" — that emits [`Event::TurnEnd`] and
    /// leaves the session alive until the caller stops it.
    stop_seen: bool,
    /// Stop reason carried by [`Event::TurnEnd`] and the trailing
    /// [`Event::Stop`]. Populated from the SDK `result.stop_reason`
    /// field on turn end, or `"eof"` when the stream closed without
    /// a terminal `result`.
    stop_reason: String,
    /// Raw SDK terminal `result` JSON object, captured for use in
    /// `AgentResult` construction. `None` if the stream closed without
    /// one (e.g. child crashed mid-stream).
    terminal_json: Option<JsonValue>,
    /// Built once the child has been reaped and finalize has run.
    final_result: Option<AgentResult>,
    /// `task_id`s of background sub-agents spawned via the `Agent` tool
    /// that have not yet reported completion. Populated from
    /// `system / subtype: task_started` and drained on `task_notification`.
    /// While non-empty, `wait()` treats per-turn `Result` (end_turn)
    /// messages as non-terminal: Claude Code auto-resumes the parent
    /// turn when each notification arrives, so we must keep reading the
    /// stream rather than interrupting the child.
    pending_tasks: HashSet<String>,
}

/// What `wait()` returns and what `result()` carries once the session
/// has terminated. Mirrors the SDK terminal `result` message
/// (`SDKResultMessage`) plus proc-level diagnostics. Structured
/// sub-objects (`usage`, `model_usage`, `permission_denials`,
/// `structured_output`) are surfaced as JSON-encoded strings; the
/// scanner author parses them with `serde::from_str` if it cares
/// about the inner shape.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub(crate) struct AgentResult {
    /// Final assistant text from the SDK `result.result` field. Empty
    /// if the child exited without emitting one.
    #[rune(get)]
    pub text: String,
    /// `result.is_error`. `false` for a normal completion.
    #[rune(get, copy)]
    pub is_error: bool,
    /// `result.stop_reason` (`"end_turn"`, `"max_tokens"`, …), or
    /// `"eof"` when the stream closed without a terminal `result`.
    #[rune(get)]
    pub stop_reason: String,
    /// `result.num_turns` — number of assistant turns in this session.
    #[rune(get, copy)]
    pub turns: i64,
    /// `result.duration_ms` — wall-clock duration of the run.
    #[rune(get, copy)]
    pub duration_ms: i64,
    /// `result.duration_api_ms` — sum of api round-trip times.
    #[rune(get, copy)]
    pub duration_api_ms: i64,
    /// `result.total_cost_usd`.
    #[rune(get, copy)]
    pub cost_usd: f64,
    /// `result.usage` JSON: input/output/cache token counts.
    #[rune(get)]
    pub usage: String,
    /// `result.modelUsage` JSON: per-model token + cost breakdown.
    #[rune(get)]
    pub model_usage: String,
    /// `result.permission_denials` JSON: tool calls that were blocked.
    #[rune(get)]
    pub permission_denials: String,
    /// `result.structured_output` JSON; empty string when the call did
    /// not use `--json-schema`.
    #[rune(get)]
    pub structured_output: String,
    /// `result.session_id` — claude's session id, also the JSONL
    /// filename under `~/.gage/claude/<name>/`.
    #[rune(get)]
    pub session_id: String,
    /// `result.uuid` — unique id for this terminal `result` event.
    #[rune(get)]
    pub uuid: String,
    /// Child exit code; `-1` if the process was signaled.
    #[rune(get, copy)]
    pub exit_code: i64,
    /// Captured stderr — diagnoses claude warnings that sit outside
    /// the JSON stream.
    #[rune(get)]
    pub stderr: String,
}

impl AgentResult {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AgentResult {{ exit_code: {}, is_error: {}, stop_reason: {:?}, \
             turns: {}, duration_ms: {}, cost_usd: {}, session_id: {:?}, \
             text: {:?}, stderr: {:?} }}",
            self.exit_code,
            self.is_error,
            self.stop_reason,
            self.turns,
            self.duration_ms,
            self.cost_usd,
            self.session_id,
            self.text,
            self.stderr,
        )?;
        Ok(())
    }

    /// Subset of fields suitable for a note's `metadata` object.
    /// `duration` is reported in seconds.
    #[rune::function(instance)]
    fn as_metadata(&self) -> Object {
        let mut obj = Object::new();
        let key = |s: &str| rune::alloc::String::try_from(s).unwrap();
        obj.insert(key("is_error"), rune::to_value(self.is_error).unwrap())
            .unwrap();
        obj.insert(
            key("stop_reason"),
            rune::to_value(self.stop_reason.clone()).unwrap(),
        )
        .unwrap();
        obj.insert(key("turns"), rune::to_value(self.turns).unwrap())
            .unwrap();
        obj.insert(
            key("duration"),
            rune::to_value(self.duration_ms as f64 / 1000.0).unwrap(),
        )
        .unwrap();
        obj.insert(key("cost_usd"), rune::to_value(self.cost_usd).unwrap())
            .unwrap();
        obj.insert(
            key("session_id"),
            rune::to_value(self.session_id.clone()).unwrap(),
        )
        .unwrap();
        obj.insert(key("exit_code"), rune::to_value(self.exit_code).unwrap())
            .unwrap();
        obj.insert(key("stderr"), rune::to_value(self.stderr.clone()).unwrap())
            .unwrap();
        obj
    }
}

/// One Rune-visible item produced by `Agent::poll`. Content-block
/// granularity: a single SDK `assistant` message carrying
/// `[text, thinking, tool_use]` expands into three `Event` values.
/// Forward-compatible types (anything the SDK adds that we haven't
/// modeled) surface as `Other(<json>)`.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub(crate) enum Event {
    /// `assistant` content block of type `text`.
    #[rune(constructor)]
    Assistant(#[rune(get)] String),
    /// `assistant` content block of type `thinking` (raw, not summarized).
    #[rune(constructor)]
    Thinking(#[rune(get)] String),
    /// `assistant` content block of type `tool_use`. `input` is the
    /// raw tool input encoded as a JSON string; the caller parses if
    /// it cares about structure.
    #[rune(constructor)]
    ToolUse {
        #[rune(get)]
        name: String,
        #[rune(get)]
        input: String,
    },
    /// `user` content block of type `tool_result`. `output` is the
    /// joined text payload (multiple text parts get concatenated).
    #[rune(constructor)]
    ToolResult {
        #[rune(get)]
        id: String,
        #[rune(get)]
        output: String,
    },
    /// `{"type": "system"}` init message, JSON-encoded for forward
    /// compatibility with new init fields the SDK adds.
    #[rune(constructor)]
    System(#[rune(get)] String),
    /// Per-turn termination. The model finished its response and the
    /// SDK emitted a `result` message with this `stop_reason`
    /// (`"end_turn"`, `"max_tokens"`, …). The session is still alive;
    /// the caller can `send`/`send_now` another user message to
    /// continue, or `stop()`/`wait()` to actually end the session.
    #[rune(constructor)]
    TurnEnd(#[rune(get)] String),
    /// Session-level termination. Fires exactly once after `stop()`
    /// has finalized the child, or when the stream closed without a
    /// terminal `result` (`reason: "eof"`). Subsequent polls return
    /// `Stop` idempotently; `result()` returns `Some`.
    #[rune(constructor)]
    Stop(#[rune(get)] String),
    /// Any SDK message `type` we haven't enumerated (status,
    /// rate_limit, hook progress, stream_event, …). JSON-encoded.
    #[rune(constructor)]
    Other(#[rune(get)] String),
    /// A stdout line that did not parse as JSON.
    #[rune(constructor)]
    ParseError(#[rune(get)] String),
}

impl Event {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        match self {
            Event::Assistant(t) => write!(f, "Assistant({t:?})")?,
            Event::Thinking(t) => write!(f, "Thinking({t:?})")?,
            Event::ToolUse { name, input } => write!(f, "ToolUse({name:?}, {input})")?,
            Event::ToolResult { id, output } => write!(f, "ToolResult({id:?}, {output:?})")?,
            Event::System(s) => write!(f, "System({s})")?,
            Event::TurnEnd(r) => write!(f, "TurnEnd({r:?})")?,
            Event::Stop(r) => write!(f, "Stop({r:?})")?,
            Event::Other(s) => write!(f, "Other({s})")?,
            Event::ParseError(s) => write!(f, "ParseError({s:?})")?,
        }
        Ok(())
    }
}

impl Agent {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Agent {{ model: {:?}, max_turns: {:?}, gage_tools: {:?}, custom_tools: {}, url: {:?} }}",
            self.spec.model,
            self.spec.max_turns,
            self.spec.gage_tools,
            self.spec.custom_tools.len(),
            self.mcp_url,
        )?;
        Ok(())
    }
}

#[rune::function(instance)]
fn url(this: &Agent) -> Option<String> {
    this.mcp_url.clone()
}

#[rune::function(instance)]
async fn poll(this: Mut<Agent>) -> super::Result<Event> {
    let inner = Arc::clone(&this.inner);
    do_poll(inner).await
}

/// Block to the session-level terminal. `Result` (a per-turn end)
/// auto-triggers `stop()` once so a one-shot `let r =
/// call_agent(...).wait().await?` works without the caller managing
/// the lifecycle. Multi-turn scanners drive `poll`/`send` and call
/// `stop` themselves; calling `wait()` after `stop()` returns the
/// cached `AgentResult` immediately.
#[rune::function(instance)]
async fn wait(this: Mut<Agent>) -> super::Result<AgentResult> {
    let inner = Arc::clone(&this.inner);
    let mut auto_stopped = false;
    loop {
        let ev = do_poll(Arc::clone(&inner)).await?;
        match ev {
            Event::Stop(_) => break,
            // Background sub-agents spawned via the `Agent` tool
            // (system / task_started) keep the session live past the
            // first `Result`: Claude Code auto-resumes the parent turn
            // when each `task_notification` arrives. Defer the
            // auto-stop until no tasks are outstanding.
            Event::TurnEnd(_)
                if !auto_stopped && inner.lock().unwrap().pending_tasks.is_empty() =>
            {
                do_stop(Arc::clone(&inner)).await?;
                auto_stopped = true;
            }
            _ => {}
        }
    }
    inner
        .lock()
        .unwrap()
        .final_result
        .clone()
        .ok_or_else(|| Error::Agent("agent.wait: result missing after Stop".into()))
}

async fn do_poll(inner: Arc<Mutex<AgentInner>>) -> super::Result<Event> {
    loop {
        tracing::trace!("agent.poll iteration");
        // Fast path: a buffered event, or an idempotent Stop replay.
        {
            let mut g = inner.lock().unwrap();
            if let Some(ev) = g.event_buf.pop_front() {
                return Ok(ev);
            }
            if g.stop_seen {
                return Ok(Event::Stop(g.stop_reason.clone()));
            }
        }

        // Slow path: pull the next SDK message off the channel. Take
        // the session out so the lock isn't held across the await; put
        // it back before processing the message.
        let mut session = inner
            .lock()
            .unwrap()
            .session
            .take()
            .ok_or_else(|| Error::Agent("agent.poll: session not available".into()))?;
        tracing::trace!("agent.poll awaiting next stream message");
        let msg = session.recv_event().await;
        tracing::debug!(
            kind = msg.as_ref().map(|m| match m {
                StreamMessage::System(_) => "system",
                StreamMessage::Assistant(_) => "assistant",
                StreamMessage::User(_) => "user",
                StreamMessage::Result(_) => "result",
                StreamMessage::Other(_) => "other",
                StreamMessage::ParseError { .. } => "parse_error",
            }),
            channel_closed = msg.is_none(),
            "agent.poll received stream message",
        );

        let eof = {
            let mut g = inner.lock().unwrap();
            g.session = Some(session);
            match msg {
                Some(StreamMessage::Result(v)) => {
                    let reason = v
                        .get("stop_reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("end_turn")
                        .to_string();
                    g.terminal_json = Some(v);
                    g.stop_reason = reason.clone();
                    g.event_buf.push_back(Event::TurnEnd(reason));
                    // If no background sub-agents are still in flight,
                    // this Result is the genuine session end — finalize
                    // so `running()` flips to false and a `while
                    // running() { poll() }` loop terminates without the
                    // caller having to call `stop()`. When tasks are
                    // pending, Claude Code auto-resumes the parent on
                    // each `task_notification`, so keep the session
                    // alive and let subsequent Results drive the same
                    // check.
                    g.pending_tasks.is_empty()
                }
                Some(other) => {
                    if let StreamMessage::System(v) = &other {
                        track_task_event(v, &mut g.pending_tasks);
                    }
                    expand_to_events(other, &mut g.event_buf);
                    false
                }
                None => {
                    // Stream closed without a turn-end marker — child
                    // exited (crashed or got SIGKILL'd). Finalize so
                    // the run dir is cleaned up and Stop fires.
                    if g.stop_reason.is_empty() {
                        g.stop_reason = "eof".to_string();
                    }
                    true
                }
            }
        };
        if eof {
            do_stop(Arc::clone(&inner)).await?;
        }
    }
}

/// Send interrupt + EOF on stdin, reap the child, run the gage-agent
/// finalize sweep, build `final_result`, and enqueue a single
/// trailing `Event::Stop`. Idempotent — second call is a no-op.
async fn do_stop(inner: Arc<Mutex<AgentInner>>) -> super::Result<()> {
    if inner.lock().unwrap().stop_seen {
        return Ok(());
    }
    tracing::debug!("agent.stop: taking session");
    let mut session = inner
        .lock()
        .unwrap()
        .session
        .take()
        .ok_or_else(|| Error::Agent("agent.stop: session not available".into()))?;

    // Best-effort interrupt; ignore BrokenPipe (the child may have
    // exited on its own already — common on the EOF path).
    if let Err(e) = session.send_interrupt().await
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        inner.lock().unwrap().session = Some(session);
        return Err(Error::Agent(format!("agent.stop: send interrupt: {e}")));
    }
    session.close_stdin();
    tracing::debug!(pid = ?session.id(), "agent.stop: stdin closed, awaiting child exit");
    let status = session
        .wait_exit()
        .await
        .map_err(|e| Error::Agent(format!("agent.stop: wait child: {e}")))?;
    tracing::debug!(?status, "agent.stop: child exited");
    session.join_stdout().await;
    let stderr_bytes = session
        .drain_stderr()
        .await
        .map_err(|e| Error::Agent(format!("agent.stop: drain stderr: {e}")))?;
    tracing::debug!(
        stderr_len = stderr_bytes.len(),
        "agent.stop: stderr drained"
    );
    session
        .finalize()
        .map_err(|e| Error::Agent(format!("agent.stop: finalize: {e}")))?;
    tracing::debug!("agent.stop: gage-agent finalize done");
    drop(session);

    let mut g = inner.lock().unwrap();
    if g.stop_reason.is_empty() {
        g.stop_reason = "eof".to_string();
    }
    let result = build_agent_result(
        g.terminal_json.as_ref(),
        &g.stop_reason,
        status.code().map(i64::from).unwrap_or(-1),
        String::from_utf8_lossy(&stderr_bytes).into_owned(),
    );
    g.final_result = Some(result);
    g.stop_seen = true;
    let reason = g.stop_reason.clone();
    g.event_buf.push_back(Event::Stop(reason));
    Ok(())
}

/// Materialize an [`AgentResult`] from the SDK terminal `result`
/// JSON, the synthetic stop reason (used when no terminal arrived),
/// child exit code, and drained stderr. Missing fields fall back to
/// defaults so a half-completed session still produces a usable
/// value.
fn build_agent_result(
    terminal: Option<&JsonValue>,
    fallback_stop_reason: &str,
    exit_code: i64,
    stderr: String,
) -> AgentResult {
    let v = terminal;
    let get_str = |k: &str| -> String {
        v.and_then(|j| j.get(k))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let get_i64 = |k: &str| -> i64 {
        v.and_then(|j| j.get(k))
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
    };
    let get_f64 = |k: &str| -> f64 {
        v.and_then(|j| j.get(k))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    let get_bool = |k: &str| -> bool {
        v.and_then(|j| j.get(k))
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
    };
    let get_json = |k: &str| -> String {
        v.and_then(|j| j.get(k))
            .map(|x| x.to_string())
            .unwrap_or_default()
    };
    let stop_reason = v
        .and_then(|j| j.get("stop_reason"))
        .and_then(|s| s.as_str())
        .unwrap_or(fallback_stop_reason)
        .to_string();
    AgentResult {
        text: get_str("result"),
        is_error: get_bool("is_error"),
        stop_reason,
        turns: get_i64("num_turns"),
        duration_ms: get_i64("duration_ms"),
        duration_api_ms: get_i64("duration_api_ms"),
        cost_usd: get_f64("total_cost_usd"),
        usage: get_json("usage"),
        model_usage: get_json("modelUsage"),
        permission_denials: get_json("permission_denials"),
        structured_output: get_json("structured_output"),
        session_id: get_str("session_id"),
        uuid: get_str("uuid"),
        exit_code,
        stderr,
    }
}

#[rune::function(instance)]
fn result(this: &Agent) -> Option<AgentResult> {
    this.inner.lock().unwrap().final_result.clone()
}

#[rune::function(instance)]
fn running(this: &Agent) -> bool {
    this.inner.lock().unwrap().final_result.is_none()
}

/// Update the pending-task set from a `system` stream-json message.
///
/// Claude Code reports background sub-agent lifecycle on the `system`
/// channel: `subtype: task_started` opens a task and `task_notification`
/// closes one (any terminal `status`). `task_updated` is informational
/// and ignored here. See `.local.notes/claude/sub-agents.md` for the
/// captured wire shape.
fn track_task_event(v: &JsonValue, pending: &mut HashSet<String>) {
    let Some(subtype) = v.get("subtype").and_then(|s| s.as_str()) else {
        return;
    };
    let Some(task_id) = v.get("task_id").and_then(|t| t.as_str()) else {
        return;
    };
    match subtype {
        "task_started" => {
            pending.insert(task_id.to_string());
        }
        "task_notification" => {
            pending.remove(task_id);
        }
        _ => {}
    }
}

/// Walk an SDK stream-json message and push one or more [`Event`]s
/// onto the buffer. The terminal `result` message is handled by the
/// caller (it drives finalize); this only sees non-terminal types.
fn expand_to_events(msg: StreamMessage, buf: &mut VecDeque<Event>) {
    match msg {
        StreamMessage::System(v) => buf.push_back(Event::System(v.to_string())),
        StreamMessage::Assistant(v) => expand_assistant(&v, buf),
        StreamMessage::User(v) => expand_user(&v, buf),
        StreamMessage::Result(_) => {
            // Terminal; the caller intercepted this before calling us.
            // Pushing nothing here keeps the buffer consistent.
        }
        StreamMessage::Other(v) => buf.push_back(Event::Other(v.to_string())),
        StreamMessage::ParseError { line, error } => {
            buf.push_back(Event::ParseError(format!("{error}: {line}")));
        }
    }
}

fn expand_assistant(v: &JsonValue, buf: &mut VecDeque<Event>) {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        buf.push_back(Event::Other(v.to_string()));
        return;
    };
    for block in content {
        let ty = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                buf.push_back(Event::Assistant(text));
            }
            "thinking" => {
                let text = block
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                buf.push_back(Event::Thinking(text));
            }
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block
                    .get("input")
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                buf.push_back(Event::ToolUse { name, input });
            }
            _ => buf.push_back(Event::Other(block.to_string())),
        }
    }
}

fn expand_user(v: &JsonValue, buf: &mut VecDeque<Event>) {
    let Some(content) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        buf.push_back(Event::Other(v.to_string()));
        return;
    };
    for block in content {
        let ty = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "tool_result" {
            let id = block
                .get("tool_use_id")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let output = tool_result_output(block);
            buf.push_back(Event::ToolResult { id, output });
        } else {
            buf.push_back(Event::Other(block.to_string()));
        }
    }
}

/// Concatenate the text payload of a tool_result content block.
/// `content` is either a string or an array of `{type:"text", text}`
/// objects; everything else stringifies the raw JSON.
fn tool_result_output(block: &JsonValue) -> String {
    let Some(content) = block.get("content") else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for part in arr {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                out.push_str(text);
            } else {
                out.push_str(&part.to_string());
            }
        }
        return out;
    }
    content.to_string()
}

#[rune::function(instance)]
async fn send(this: Mut<Agent>, msg: String) -> super::Result<()> {
    let inner = Arc::clone(&this.inner);
    let mut session = inner
        .lock()
        .unwrap()
        .session
        .take()
        .ok_or_else(|| Error::Agent("agent.send: session not available".into()))?;
    let res = session.send_user_message(&msg).await;
    inner.lock().unwrap().session = Some(session);
    res.map_err(|e| Error::Agent(format!("agent.send: {e}")))
}

/// Interrupt the current turn and queue `msg` as the next user
/// message. Sends a `control_request` interrupt on stdin, then the
/// user message — stdin stays open so the session continues past
/// the interrupt.
#[rune::function(instance)]
async fn send_now(this: Mut<Agent>, msg: String) -> super::Result<()> {
    let inner = Arc::clone(&this.inner);
    let mut session = inner
        .lock()
        .unwrap()
        .session
        .take()
        .ok_or_else(|| Error::Agent("agent.send_now: session not available".into()))?;
    let res = async {
        session.send_interrupt().await?;
        session.send_user_message(&msg).await
    }
    .await;
    inner.lock().unwrap().session = Some(session);
    res.map_err(|e| Error::Agent(format!("agent.send_now: {e}")))
}

/// End the session: send an `interrupt` control-request, close
/// stdin, reap the child, run the gage-agent finalize sweep
/// (archive, WAL replay, run-dir cleanup), and queue `Event::Stop`.
/// Idempotent — calling `stop` after the session has already ended
/// is a no-op. `result()` returns `Some` once this completes.
#[rune::function(instance)]
async fn stop(this: Mut<Agent>) -> super::Result<()> {
    do_stop(Arc::clone(&this.inner)).await
}

#[rune::function(instance)]
async fn kill(this: Mut<Agent>, grace_secs: i64) -> super::Result<()> {
    let inner = Arc::clone(&this.inner);
    let mut session = inner
        .lock()
        .unwrap()
        .session
        .take()
        .ok_or_else(|| Error::Agent("agent.kill: session not available".into()))?;
    let grace = std::time::Duration::from_secs(grace_secs.max(0) as u64);
    let res = session.kill(grace).await;
    inner.lock().unwrap().session = Some(session);
    res.map_err(|e| Error::Agent(format!("agent.kill: {e}")))
}

impl CallAgent {
    #[rune::function(instance)]
    fn model(mut self, model: String) -> Self {
        self.model = Some(model);
        self
    }

    #[rune::function(instance)]
    fn max_turns(mut self, max_turns: i64) -> Self {
        self.max_turns = Some(max_turns.max(0) as u32);
        self
    }

    #[rune::function(instance)]
    fn timeout(mut self, timeout: i64) -> Self {
        self.timeout_secs = Some(timeout.max(0) as u64);
        self
    }

    #[rune::function(instance)]
    fn system_prompt(mut self, s: String) -> Self {
        self.system_prompt = Some(s);
        self
    }

    #[rune::function(instance)]
    fn append_system_prompt(mut self, s: String) -> Self {
        self.append_system_prompt = Some(s);
        self
    }

    #[rune::function(instance)]
    fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    #[rune::function(instance)]
    fn gage_tools(mut self, list: Value) -> Self {
        self.gage_tools = Some(parse_gage_tools(list));
        self
    }

    #[rune::function(instance)]
    fn tools(mut self, map: Value) -> Self {
        self.custom_tools = Some(parse_custom_tools(map));
        self
    }
}

fn parse_gage_tools(v: Value) -> super::Result<GageTools> {
    let items: rune::runtime::Vec = FromValue::from_value(v)
        .map_err(|e| Error::Agent(format!("'gage_tools' must be a list of strings: {e}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        let s: String = FromValue::from_value(item.clone())
            .map_err(|e| Error::Agent(format!("'gage_tools' must be a list of strings: {e}")))?;
        out.push(s);
    }
    if out.iter().any(|s| s == "*") {
        if out.len() != 1 {
            return Err(Error::Agent(
                "'gage_tools': \"*\" must be the only entry".into(),
            ));
        }
        return Ok(GageTools::All);
    }
    Ok(GageTools::Some(out))
}

fn parse_custom_tools(v: Value) -> super::Result<Vec<CustomToolDef>> {
    let obj: Object = FromValue::from_value(v).map_err(|e| {
        Error::Agent(format!(
            "'tools' must be an object keyed by function name: {e}"
        ))
    })?;
    // Sort by key so the resulting tool order is deterministic.
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (k, val) in obj.iter() {
        entries.insert(k.to_string(), val.clone());
    }
    let mut out = Vec::with_capacity(entries.len());
    for (fn_name, decl) in entries {
        out.push(parse_one_custom_tool(fn_name, decl)?);
    }
    Ok(out)
}

fn parse_one_custom_tool(fn_name: String, decl: Value) -> super::Result<CustomToolDef> {
    let obj: Object = FromValue::from_value(decl).map_err(|e| {
        Error::Agent(format!(
            "'tools.{fn_name}' must be an object {{ name, description, inputs }}: {e}"
        ))
    })?;
    let mcp_name = pop_string(&obj, "name", &fn_name)?;
    let description = pop_string(&obj, "description", &fn_name)?;
    let inputs = match obj.get(&rune::alloc::String::try_from("inputs").unwrap()) {
        Some(v) => parse_inputs(v.clone(), &fn_name)?,
        None => Vec::new(),
    };
    Ok(CustomToolDef {
        fn_name,
        mcp_name,
        description,
        inputs,
    })
}

fn pop_string(obj: &Object, key: &str, fn_name: &str) -> super::Result<String> {
    let v = obj
        .get(&rune::alloc::String::try_from(key).unwrap())
        .ok_or_else(|| Error::Agent(format!("'tools.{fn_name}.{key}' is required")))?;
    String::from_value(v.clone())
        .map_err(|e| Error::Agent(format!("'tools.{fn_name}.{key}' must be a string: {e}")))
}

fn parse_inputs(v: Value, fn_name: &str) -> super::Result<Vec<InputDecl>> {
    let obj: Object = FromValue::from_value(v).map_err(|e| {
        Error::Agent(format!(
            "'tools.{fn_name}.inputs' must be an object keyed by input name: {e}"
        ))
    })?;
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (k, val) in obj.iter() {
        entries.insert(k.to_string(), val.clone());
    }
    let mut out = Vec::with_capacity(entries.len());
    for (name, decl) in entries {
        out.push(parse_one_input(fn_name, name, decl)?);
    }
    Ok(out)
}

fn parse_one_input(fn_name: &str, name: String, decl: Value) -> super::Result<InputDecl> {
    let obj: Object = FromValue::from_value(decl).map_err(|e| {
        Error::Agent(format!(
            "'tools.{fn_name}.inputs.{name}' must be an object \
             {{ type, required?, description? }}: {e}"
        ))
    })?;
    let type_str = match obj.get(&rune::alloc::String::try_from("type").unwrap()) {
        Some(v) => String::from_value(v.clone()).map_err(|e| {
            Error::Agent(format!(
                "'tools.{fn_name}.inputs.{name}.type' must be a string: {e}"
            ))
        })?,
        None => "string".to_string(),
    };
    let required = match obj.get(&rune::alloc::String::try_from("required").unwrap()) {
        Some(v) => bool::from_value(v.clone()).map_err(|e| {
            Error::Agent(format!(
                "'tools.{fn_name}.inputs.{name}.required' must be a bool: {e}"
            ))
        })?,
        None => true,
    };
    let description = match obj.get(&rune::alloc::String::try_from("description").unwrap()) {
        Some(v) => Some(String::from_value(v.clone()).map_err(|e| {
            Error::Agent(format!(
                "'tools.{fn_name}.inputs.{name}.description' must be a string: {e}"
            ))
        })?),
        None => None,
    };
    Ok(InputDecl {
        name,
        type_str,
        required,
        description,
    })
}

#[rune::function]
fn call_agent(prompt: String) -> CallAgent {
    CallAgent {
        prompt,
        model: None,
        max_turns: None,
        timeout_secs: None,
        system_prompt: None,
        append_system_prompt: None,
        name: None,
        gage_tools: None,
        custom_tools: None,
    }
}

async fn do_call_agent(c: CallAgent) -> super::Result<Agent> {
    let CallAgent {
        prompt,
        model,
        max_turns,
        timeout_secs,
        system_prompt,
        append_system_prompt,
        name,
        gage_tools,
        custom_tools,
    } = c;
    let gage_tools = match gage_tools {
        Some(r) => r?,
        None => GageTools::None,
    };
    let custom_tools = match custom_tools {
        Some(r) => r?,
        None => Vec::new(),
    };
    let spec = Arc::new(CallSpec {
        prompt,
        model,
        max_turns,
        timeout_secs,
        system_prompt,
        append_system_prompt,
        gage_tools,
        custom_tools,
        name,
    });

    // Acquire a slot from the run-wide agent pool BEFORE doing any
    // expensive setup (MCP register, sandbox materialization, child
    // spawn). If the pool is saturated this awaits until a previously
    // running agent finishes; resource use stays bounded by the user's
    // `--agent-jobs`.
    let pool = current_scan_ctx().run.agent_pool.clone();
    let permit = pool
        .acquire_owned()
        .await
        .expect("agent_pool is closed only at process shutdown");

    let tool_spec = build_tool_spec(&spec);
    let gage_tools_resolved = tool_spec.gage_tools.clone();
    let (mcp_url, service) = if tool_spec.gage_tools.is_empty() && tool_spec.custom_tools.is_empty()
    {
        // Pure text-in/text-out call — no need to register an MCP
        // service or even reach the host.
        (None, None)
    } else {
        let ctx = current_scan_ctx();
        let host = ctx.run.mcp_host.clone().ok_or_else(|| {
            Error::Agent(
                "call_agent: tools declared but no MCP host available in this run \
                 (host startup failed?)"
                    .into(),
            )
        })?;
        let svc = host.register(gage_mcp::build_mcp_service(tool_spec));
        (Some(svc.url().to_string()), Some(svc))
    };

    let scan_id = current_scan_ctx().run.scan_id.clone();
    // `settings.json` permissions.allow must include both built-in
    // Gage tools and scanner-defined custom tools — otherwise claude
    // prompts the user on every custom-tool call, which in `-p`
    // headless mode means the call is rejected.
    let mut auto_allow = gage_tools_resolved;
    for def in &spec.custom_tools {
        auto_allow.push(def.mcp_name.clone());
    }
    let mut builder = GageAgentBuilder::new().tools(auto_allow).scan_id(&scan_id);
    if let Some(n) = &spec.name {
        builder = builder.name(n.clone());
    }
    // Sandbox materialization gated by tool declarations: no tools →
    // pure text-in/text-out → skip the heavy sqlite + session
    // hardlink setup. Tools declared → restrict to the running scan's
    // rows.
    if mcp_url.is_some() {
        let sandbox = scan_sandbox_spec(&scan_id).map_err(Error::Agent)?;
        builder = builder.sandbox(sandbox);
    } else {
        builder = builder.no_sandbox();
    }
    if let Some(url) = &mcp_url {
        builder = builder.mcp_url(url.clone());
    }
    if let Some(m) = &spec.model {
        // Preserve `call_llm`'s alias support — `"small"`/`"medium"`/
        // `"large"` round-trip to concrete claude model ids so existing
        // scanner code keeps working unchanged.
        builder = builder.model(crate::model::resolve_model(m).to_string());
    }
    if let Some(n) = spec.max_turns {
        builder = builder.max_turns(n);
    }
    if let Some(t) = spec.timeout_secs {
        builder = builder.timeout(t as usize);
    }
    let session = builder
        .build()
        .start_streaming_session(&spec.prompt)
        .await
        .map_err(|e| Error::Agent(format!("call_agent: start_streaming_session: {e}")))?;

    Ok(Agent {
        spec,
        mcp_url,
        _service: service,
        _permit: permit,
        inner: Arc::new(Mutex::new(AgentInner {
            session: Some(session),
            event_buf: VecDeque::new(),
            stop_seen: false,
            stop_reason: String::new(),
            terminal_json: None,
            final_result: None,
            pending_tasks: HashSet::new(),
        })),
    })
}

/// Restrict the agent's sandbox to the running scan's rows. Reads the
/// canonical gage db once for the scan-session / scan-note / scan-issue
/// sets that already exist.
fn scan_sandbox_spec(scan_id: &str) -> Result<SandboxSpec, String> {
    let conn = gage_db::db::open_db().map_err(|e| e.to_string())?;
    let sessions: HashSet<String> = gage_db::scan::session_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let notes: HashSet<String> = gage_db::scan::note_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let issues: HashSet<String> = gage_db::scan::issue_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    Ok(SandboxSpec {
        sessions: Some(sessions),
        notes: Some(notes),
        issues: Some(issues),
        scan: Some(scan_id.to_string()),
    })
}

/// Translate the resolved [`CallSpec`] into [`gage_mcp::ToolSpec`].
///
/// `GageTools::All` expands against [`gage_agent::TOOL_NAMES`];
/// `GageTools::Some(list)` passes through verbatim; `GageTools::None`
/// yields an empty `gage_tools`. Each custom tool becomes a
/// [`gage_mcp::CustomToolDef`] whose callback spawns a fresh
/// `rune::Vm` over the calling task's compiled unit, scoped to the
/// same `ScanContext` — see "Scanner-tool callback execution" in the
/// design doc.
fn build_tool_spec(spec: &CallSpec) -> ToolSpec {
    let gage_tools = match &spec.gage_tools {
        GageTools::None => Vec::new(),
        GageTools::Some(list) => list.clone(),
        GageTools::All => gage_agent::TOOL_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let ctx = current_scan_ctx();
    let dispatcher_sender = ctx.run.dispatcher.get().map(|d| d.sender());
    let module_id = ctx.scanner_name.clone();
    let custom_tools = spec
        .custom_tools
        .iter()
        .map(|def| gage_mcp::CustomToolDef {
            name: def.mcp_name.clone(),
            description: def.description.clone(),
            input_schema: render_input_schema(&def.inputs),
            callback: dispatcher_callback(
                module_id.clone(),
                def.fn_name.clone(),
                dispatcher_sender.clone(),
            ),
        })
        .collect();
    ToolSpec {
        gage_tools,
        custom_tools,
    }
}

/// Render `InputDecl[]` to the MCP `{"type": "object", "properties":
/// {...}, "required": [...]}` schema.
fn render_input_schema(inputs: &[InputDecl]) -> rmcp_json::JsonObject {
    let mut properties = rmcp_json::JsonObject::new();
    let mut required: Vec<JsonValue> = Vec::new();
    for inp in inputs {
        let mut prop = rmcp_json::JsonObject::new();
        prop.insert("type".into(), JsonValue::String(inp.type_str.clone()));
        if let Some(desc) = &inp.description {
            prop.insert("description".into(), JsonValue::String(desc.clone()));
        }
        properties.insert(inp.name.clone(), JsonValue::Object(prop));
        if inp.required {
            required.push(JsonValue::String(inp.name.clone()));
        }
    }
    let mut obj = rmcp_json::JsonObject::new();
    obj.insert("type".into(), JsonValue::String("object".into()));
    obj.insert("properties".into(), JsonValue::Object(properties));
    obj.insert("required".into(), JsonValue::Array(required));
    obj
}

/// Build the MCP callback for a scanner-defined tool. The closure
/// sends a JSON-only [`DispatchRequest`] to the dispatcher and waits
/// for the reply — no Rune state crosses the channel.
fn dispatcher_callback(
    module_id: String,
    fn_name: String,
    sender: Option<tokio::sync::mpsc::UnboundedSender<crate::dispatcher::DispatchRequest>>,
) -> CustomToolCallback {
    Arc::new(move |args| {
        let module_id = module_id.clone();
        let fn_name = fn_name.clone();
        let sender = sender.clone();
        Box::pin(async move {
            let sender = sender.ok_or_else(|| {
                "tool dispatcher unavailable for this run (startup failed?)".to_string()
            })?;
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            sender
                .send(crate::dispatcher::DispatchRequest {
                    module_id,
                    fn_name,
                    args,
                    reply: reply_tx,
                })
                .map_err(|_send_err| "tool dispatcher channel closed".to_string())?;
            reply_rx
                .await
                .map_err(|_recv_err| "tool dispatcher dropped reply".to_string())?
        })
    })
}

/// `rmcp::model::JsonObject` re-exported via `gage_mcp::CustomToolDef`'s
/// `input_schema` field type — alias kept local so this module doesn't
/// import the rmcp crate just for one type name.
mod rmcp_json {
    pub type JsonObject = serde_json::Map<String, serde_json::Value>;
}

/// Queue side of the concurrent agent driver. Built by the scanner via
/// `AgentRunner::new()`, fed with `add(call_agent, context)`, and turned
/// into an [`AgentRunnerResults`] via `start()`. Concurrency across all
/// `call_agent` invocations (including those routed through `AgentRunner`)
/// is bounded by the run-wide `agent_pool` semaphore configured via the
/// CLI's `--agent-jobs` flag; the runner itself does not impose a separate
/// cap.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct AgentRunner {
    #[rune(skip)]
    queue: Vec<(CallAgent, Object)>,
}

/// Consumer side returned by [`AgentRunner::start`]. Each call to
/// `next().await` yields the next completed run in completion order, or
/// `None` once every queued agent has finished. Every queued future
/// acquires its permit from the shared `agent_pool` inside
/// `do_call_agent`, so the in-flight set is bounded by `--agent-jobs`.
/// The futures execute inline on the scanner's task as it polls
/// `next()`.
type RunFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = super::Result<(AgentResult, Object)>>>>;

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct AgentRunnerResults {
    #[rune(skip)]
    futures: futures::stream::FuturesUnordered<RunFuture>,
}

impl AgentRunner {
    #[rune::function(path = Self::new)]
    fn new() -> AgentRunner {
        AgentRunner { queue: Vec::new() }
    }
}

#[rune::function(instance, path = add)]
fn runner_add(mut this: Mut<AgentRunner>, call: CallAgent, ctx: Object) {
    this.queue.push((call, ctx));
}

/// Consume the runner and build its concurrent driver. Each queued call
/// becomes one future in a `FuturesUnordered`; permit acquisition for
/// the run-wide `agent_pool` happens inside `do_call_agent`, so only
/// `--agent-jobs` claude processes spawn concurrently. The futures
/// execute inline on the caller's task as it polls `next()`.
#[rune::function(instance, path = start)]
fn runner_start(this: AgentRunner) -> AgentRunnerResults {
    let futures = futures::stream::FuturesUnordered::new();
    for (call, ctx) in this.queue {
        let fut: RunFuture = Box::pin(async move {
            let result = call_and_wait(call).await;
            if let Err(e) = &result {
                tracing::warn!(error = %e, "AgentRunner: agent run failed");
            }
            result.map(|r| (r, ctx))
        });
        futures.push(fut);
    }
    AgentRunnerResults { futures }
}

#[rune::function(instance, path = next)]
async fn runner_results_next(
    mut this: Mut<AgentRunnerResults>,
) -> Option<super::Result<(AgentResult, Object)>> {
    use futures::stream::StreamExt;
    this.futures.next().await
}

/// Drive a `CallAgent` from start to its terminal `AgentResult`. Mirrors
/// `wait()`'s loop: auto-stop on the first `TurnEnd` once no background
/// sub-agents are pending.
async fn call_and_wait(call: CallAgent) -> super::Result<AgentResult> {
    let agent = do_call_agent(call).await?;
    let inner = Arc::clone(&agent.inner);
    let mut auto_stopped = false;
    loop {
        let ev = do_poll(Arc::clone(&inner)).await?;
        match ev {
            Event::Stop(_) => break,
            Event::TurnEnd(_)
                if !auto_stopped && inner.lock().unwrap().pending_tasks.is_empty() =>
            {
                do_stop(Arc::clone(&inner)).await?;
                auto_stopped = true;
            }
            _ => {}
        }
    }
    let result = inner
        .lock()
        .unwrap()
        .final_result
        .clone()
        .ok_or_else(|| Error::Agent("agent.wait: result missing after Stop".into()))?;
    drop(agent);
    Ok(result)
}

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Agent>()?;
    m.function_meta(Agent::debug)?;
    m.ty::<AgentResult>()?;
    m.function_meta(AgentResult::debug)?;
    m.function_meta(AgentResult::as_metadata)?;
    m.ty::<Event>()?;
    m.function_meta(Event::debug)?;
    m.function_meta(url)?;
    m.function_meta(poll)?;
    m.function_meta(wait)?;
    m.function_meta(result)?;
    m.function_meta(running)?;
    m.function_meta(send)?;
    m.function_meta(send_now)?;
    m.function_meta(stop)?;
    m.function_meta(kill)?;

    m.ty::<AgentRunner>()?;
    m.function_meta(AgentRunner::new)?;
    m.function_meta(runner_add)?;
    m.function_meta(runner_start)?;
    m.ty::<AgentRunnerResults>()?;
    m.function_meta(runner_results_next)?;

    m.ty::<CallAgent>()?;
    m.function_meta(CallAgent::model)?;
    m.function_meta(CallAgent::max_turns)?;
    m.function_meta(CallAgent::timeout)?;
    m.function_meta(CallAgent::system_prompt)?;
    m.function_meta(CallAgent::append_system_prompt)?;
    m.function_meta(CallAgent::name)?;
    m.function_meta(CallAgent::gage_tools)?;
    m.function_meta(CallAgent::tools)?;
    m.function_meta(call_agent)?;
    m.associated_function(&Protocol::INTO_FUTURE, |c: CallAgent| async move {
        do_call_agent(c).await
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use rune::Module;

    #[test]
    fn registers_into_module() {
        let mut m = Module::with_crate("gage").unwrap();
        super::register(&mut m).unwrap();
    }
}
