//! `call_agent(prompt)`: run an agent through the scan's driver.
//!
//! The builder collects the call's shape and, when awaited, starts the
//! agent through [`Driver::run_agent`](gage_session::Driver::run_agent)
//! and returns an [`Agent`]. `poll` yields one [`Event`] at a time;
//! `wait` drives the agent to its end and returns the
//! [`AgentResult`]; `send`, `send_now`, `stop`, and `kill` steer a
//! running agent. A turn end with no background work outstanding ends
//! the session on its own, so `call_agent(p).await?.wait().await?` is
//! the one-shot form. Each event is delivered once; a `poll` after
//! `Stop`, or a `send`, `send_now`, or `kill` after the session has
//! ended, fails with `AgentError::Stopped`.
//!
//! When the agent ends, the runtime stores its transcript as a session
//! object whose `native_source` is
//! `session+task:<scan id>/<scanner>:<task>/<native id>`, writes the
//! agent's record into the scan directory under the task, and has the
//! driver remove the run's working files.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gage_runtime::error::{AgentError, Error};
use gage_session::{AgentEvent, AgentOutcome, AgentSession, AgentSpec, SystemPrompt};
use gage_store::{AgentAttrs, SessionStore};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Mut, Object, Protocol, Ref, VmError};
use rune::{Any, ContextError, Module};

use crate::scan::{ScanContext, current};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.ty::<CallAgent>()?;
    m.function_meta(call_agent)?;
    m.function_meta(CallAgent::model)?;
    m.function_meta(CallAgent::max_turns)?;
    m.function_meta(CallAgent::timeout)?;
    m.function_meta(CallAgent::system_prompt)?;
    m.function_meta(CallAgent::default_system_prompt)?;
    m.function_meta(CallAgent::default_system_prompt_append)?;
    m.function_meta(CallAgent::name)?;
    m.associated_function(&Protocol::INTO_FUTURE, |c: CallAgent| async move {
        Ok::<_, VmError>(start(c).await)
    })?;

    m.ty::<Agent>()?;
    m.function_meta(Agent::debug)?;
    m.function_meta(poll)?;
    m.function_meta(wait)?;
    m.function_meta(result)?;
    m.function_meta(running)?;
    m.function_meta(send)?;
    m.function_meta(send_now)?;
    m.function_meta(stop)?;
    m.function_meta(kill)?;

    m.ty::<AgentResult>()?;
    m.function_meta(AgentResult::debug)?;
    m.function_meta(AgentResult::as_metadata)?;

    m.ty::<Event>()?;
    m.function_meta(Event::debug)?;
    Ok(m)
}

/// The value of `call_agent(prompt)`. Awaiting it starts the agent.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct CallAgent {
    #[rune(skip)]
    spec: AgentSpec,
}

#[rune::function]
fn call_agent(prompt: Ref<str>) -> CallAgent {
    CallAgent {
        spec: AgentSpec {
            prompt: prompt.to_owned(),
            model: None,
            system_prompt: SystemPrompt::Empty,
            system_prompt_append: None,
            max_turns: None,
            timeout: None,
            project: None,
        },
    }
}

impl CallAgent {
    /// A model alias (`small`, `medium`, `large`) or a name the driver
    /// accepts
    #[rune::function(instance)]
    fn model(mut self, model: Ref<str>) -> Self {
        self.spec.model = Some(model.to_owned());
        self
    }

    #[rune::function(instance)]
    fn max_turns(mut self, max_turns: i64) -> Self {
        self.spec.max_turns = Some(max_turns.max(0) as u32);
        self
    }

    /// Seconds to wait for the agent process to exit when the session
    /// is ended
    #[rune::function(instance)]
    fn timeout(mut self, seconds: i64) -> Self {
        self.spec.timeout = Some(Duration::from_secs(seconds.max(0) as u64));
        self
    }

    /// Replace the system prompt
    #[rune::function(instance)]
    fn system_prompt(mut self, s: Ref<str>) -> Self {
        self.spec.system_prompt = SystemPrompt::Custom(s.to_owned());
        self
    }

    /// Use the harness's default system prompt instead of none
    #[rune::function(instance)]
    fn default_system_prompt(mut self) -> Self {
        self.spec.system_prompt = SystemPrompt::Default;
        self
    }

    /// Append to the harness's default system prompt, which this
    /// selects
    #[rune::function(instance)]
    fn default_system_prompt_append(mut self, s: Ref<str>) -> Self {
        self.spec.system_prompt = SystemPrompt::Default;
        self.spec.system_prompt_append = Some(s.to_owned());
        self
    }

    /// The project name recorded on the stored transcript
    #[rune::function(instance)]
    fn name(mut self, name: Ref<str>) -> Self {
        self.spec.project = Some(name.to_owned());
        self
    }
}

/// Start the agent through the scan's driver.
async fn start(c: CallAgent) -> Result<Agent, Error> {
    let ctx = current().map_err(|e| Error::agent(e.to_string()))?;
    let model = c.spec.model.clone();
    let max_turns = c.spec.max_turns;
    let session = ctx
        .driver
        .run_agent(c.spec)
        .map_err(|e| Error::agent(format!("call_agent: {e}")))?;
    Ok(Agent {
        model,
        max_turns,
        inner: Arc::new(Mutex::new(AgentInner {
            session: Some(session),
            ctx,
            event_buf: VecDeque::new(),
            stop_seen: false,
            stop_reason: String::new(),
            last_outcome: None,
            final_result: None,
        })),
    })
}

/// A running agent. State sits under one sync mutex the methods lock
/// briefly; the driver's session is taken out of it for each await
/// and put back afterwards.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Agent {
    #[rune(skip)]
    model: Option<String>,
    #[rune(skip)]
    max_turns: Option<u32>,
    #[rune(skip)]
    inner: Arc<Mutex<AgentInner>>,
}

struct AgentInner {
    /// `None` while a method holds the session across an await, and
    /// after the session has ended
    session: Option<Box<dyn AgentSession>>,
    ctx: ScanContext,
    event_buf: VecDeque<Event>,
    stop_seen: bool,
    /// The last turn's stop reason, `eof` when the harness closed its
    /// output without one
    stop_reason: String,
    last_outcome: Option<AgentOutcome>,
    final_result: Option<AgentResult>,
}

type Inner = Arc<Mutex<AgentInner>>;

impl Agent {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Agent {{ model: {:?}, max_turns: {:?} }}",
            self.model, self.max_turns
        )?;
        Ok(())
    }
}

/// The next event. `Stop` is the last; a later call fails with
/// `AgentError::Stopped`.
#[rune::function(instance)]
async fn poll(this: Mut<Agent>) -> Result<Result<Event, Error>, VmError> {
    Ok(do_poll(Arc::clone(&this.inner)).await)
}

/// Drive the agent to its end, consuming its events, and return its
/// result. A turn end with no background work outstanding ends the
/// session; a caller that wants more turns drives `poll` and `send`
/// and calls `stop` itself. After the session has ended, the result
/// again.
#[rune::function(instance)]
async fn wait(this: Mut<Agent>) -> Result<Result<AgentResult, Error>, VmError> {
    let inner = Arc::clone(&this.inner);
    Ok(async {
        loop {
            {
                let g = inner.lock().unwrap();
                if g.stop_seen && g.event_buf.is_empty() {
                    break;
                }
            }
            do_poll(Arc::clone(&inner)).await?;
        }
        inner
            .lock()
            .unwrap()
            .final_result
            .clone()
            .ok_or_else(|| Error::agent("agent.wait: result missing after Stop"))
    }
    .await)
}

async fn do_poll(inner: Inner) -> Result<Event, Error> {
    loop {
        {
            let mut g = inner.lock().unwrap();
            if let Some(ev) = g.event_buf.pop_front() {
                return Ok(ev);
            }
            if g.stop_seen {
                return Err(Error::Agent(AgentError::Stopped));
            }
        }
        let mut session = take_session(&inner, "agent.poll")?;
        let next = session.next_event().await;
        let ended = {
            let mut g = inner.lock().unwrap();
            g.session = Some(session);
            match next {
                Some(AgentEvent::TurnEnd { outcome, idle }) => {
                    g.stop_reason = outcome.stop_reason.clone();
                    g.event_buf
                        .push_back(Event::TurnEnd(outcome.stop_reason.clone()));
                    g.last_outcome = Some(*outcome);
                    idle
                }
                Some(ev) => {
                    g.event_buf.push_back(Event::from(ev));
                    false
                }
                None => {
                    if g.stop_reason.is_empty() {
                        g.stop_reason = "eof".to_string();
                    }
                    true
                }
            }
        };
        if ended {
            do_stop(Arc::clone(&inner)).await?;
        }
    }
}

fn take_session(inner: &Inner, what: &str) -> Result<Box<dyn AgentSession>, Error> {
    let mut g = inner.lock().unwrap();
    if g.stop_seen {
        return Err(Error::Agent(AgentError::Stopped));
    }
    g.session
        .take()
        .ok_or_else(|| Error::agent(format!("{what}: session not available")))
}

/// End the session: interrupt, close input, reap the process, store
/// the transcript, write the task's agent record, clean up, and queue
/// `Stop`. A second call is a no-op.
async fn do_stop(inner: Inner) -> Result<(), Error> {
    if inner.lock().unwrap().stop_seen {
        return Ok(());
    }
    let mut session = take_session(&inner, "agent.stop")?;
    if let Err(e) = session.interrupt().await
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        inner.lock().unwrap().session = Some(session);
        return Err(Error::agent(format!("agent.stop: interrupt: {e}")));
    }
    session.close_input();
    let exit_code = session
        .wait_exit()
        .await
        .map_err(|e| Error::agent(format!("agent.stop: wait: {e}")))?;
    let stderr = session
        .take_stderr()
        .await
        .map_err(|e| Error::agent(format!("agent.stop: stderr: {e}")))?;
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    let (ctx, outcome, stop_reason) = {
        let g = inner.lock().unwrap();
        (g.ctx.clone(), g.last_outcome.clone(), g.stop_reason.clone())
    };
    store_transcript(&ctx, session.as_mut(), exit_code, &stderr, outcome.as_ref()).await?;
    if let Err(e) = session.cleanup() {
        tracing::warn!(error = %e, "agent cleanup");
    }
    drop(session);

    let mut g = inner.lock().unwrap();
    g.final_result = Some(AgentResult::new(
        outcome.unwrap_or_default(),
        &stop_reason,
        exit_code,
        stderr,
    ));
    g.stop_seen = true;
    g.event_buf.push_back(Event::Stop(stop_reason));
    Ok(())
}

/// Add the transcript to the store under the task's `session+task:`
/// source and write the task's agent record into the scan directory.
async fn store_transcript(
    ctx: &ScanContext,
    session: &mut dyn AgentSession,
    exit_code: i32,
    stderr: &str,
    outcome: Option<&AgentOutcome>,
) -> Result<(), Error> {
    let store = ctx.store.lock().await;
    let Some(mut native) = session
        .transcript()
        .map_err(|e| Error::agent(format!("agent transcript: {e}")))?
    else {
        tracing::warn!("agent wrote no transcript; nothing stored");
        return Ok(());
    };
    let source = format!(
        "session+task:{}/{}:{}/{}",
        ctx.scan_id,
        ctx.scanner,
        ctx.task,
        native.id()
    );
    let added = SessionStore::from(&*store)
        .with_native_source(source)
        .add(&*ctx.driver, &mut *native)
        .map_err(|e| Error::agent(format!("storing agent session: {e}")))?;
    drop(store);
    tracing::info!(session = %added.id, "stored agent session");
    write_agent_record(
        &ctx.paths.tasks_dir.join(&ctx.scanner).join(&ctx.task),
        &added.id,
        &added.commit_sha,
        exit_code,
        stderr,
        outcome,
    )
    .map_err(|e| Error::agent(format!("writing agent record: {e}")))
}

/// `agents/<id>/{attrs.json, stderr, result}` under the task's
/// directory, and the session's commit appended to `agents.link`.
fn write_agent_record(
    task_dir: &Path,
    id: &str,
    commit_sha: &str,
    exit_code: i32,
    stderr: &str,
    outcome: Option<&AgentOutcome>,
) -> io::Result<()> {
    let dir = task_dir.join("agents").join(id);
    fs::create_dir_all(&dir)?;
    let attrs = serde_json::to_vec_pretty(&AgentAttrs {
        exit_code: i64::from(exit_code),
    })
    .map_err(io::Error::other)?;
    fs::write(dir.join("attrs.json"), attrs)?;
    fs::write(dir.join("stderr"), stderr)?;
    if let Some(outcome) = outcome {
        fs::write(dir.join("result"), &outcome.raw)?;
    }
    let mut link = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(task_dir.join("agents.link"))?;
    writeln!(link, "{commit_sha}")
}

/// The result once the session has ended, `None` while it runs
#[rune::function(instance)]
fn result(this: &Agent) -> Option<AgentResult> {
    this.inner.lock().unwrap().final_result.clone()
}

#[rune::function(instance)]
fn running(this: &Agent) -> bool {
    this.inner.lock().unwrap().final_result.is_none()
}

/// Queue a user message after the current turn
#[rune::function(instance)]
async fn send(this: Mut<Agent>, msg: Ref<str>) -> Result<Result<(), Error>, VmError> {
    let inner = Arc::clone(&this.inner);
    Ok(async {
        let mut session = take_session(&inner, "agent.send")?;
        let res = session.send(&msg).await;
        inner.lock().unwrap().session = Some(session);
        res.map_err(|e| Error::agent(format!("agent.send: {e}")))
    }
    .await)
}

/// Interrupt the current turn and send `msg` in its place
#[rune::function(instance)]
async fn send_now(this: Mut<Agent>, msg: Ref<str>) -> Result<Result<(), Error>, VmError> {
    let inner = Arc::clone(&this.inner);
    Ok(async {
        let mut session = take_session(&inner, "agent.send_now")?;
        let res = async {
            session.interrupt().await?;
            session.send(&msg).await
        }
        .await;
        inner.lock().unwrap().session = Some(session);
        res.map_err(|e| Error::agent(format!("agent.send_now: {e}")))
    }
    .await)
}

/// End the session. `result()` is `Some` afterwards.
#[rune::function(instance)]
async fn stop(this: Mut<Agent>) -> Result<Result<(), Error>, VmError> {
    Ok(do_stop(Arc::clone(&this.inner)).await)
}

/// Terminate the process, forcibly after `grace_secs`
#[rune::function(instance)]
async fn kill(this: Mut<Agent>, grace_secs: i64) -> Result<Result<(), Error>, VmError> {
    let inner = Arc::clone(&this.inner);
    Ok(async {
        let mut session = take_session(&inner, "agent.kill")?;
        let res = session
            .kill(Duration::from_secs(grace_secs.max(0) as u64))
            .await;
        inner.lock().unwrap().session = Some(session);
        res.map_err(|e| Error::agent(format!("agent.kill: {e}")))
    }
    .await)
}

/// What the agent produced, once it has ended.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct AgentResult {
    /// The final assistant text; empty when the harness reported none
    #[rune(get)]
    pub text: String,
    #[rune(get, copy)]
    pub is_error: bool,
    /// The last turn's stop reason, or `eof` when the harness closed
    /// its output without one
    #[rune(get)]
    pub stop_reason: String,
    #[rune(get, copy)]
    pub turns: i64,
    #[rune(get, copy)]
    pub duration_ms: i64,
    #[rune(get, copy)]
    pub duration_api_ms: i64,
    #[rune(get, copy)]
    pub cost_usd: f64,
    /// Token counts, JSON
    #[rune(get)]
    pub usage: String,
    /// Per-model breakdown, JSON
    #[rune(get)]
    pub model_usage: String,
    /// Blocked tool calls, JSON
    #[rune(get)]
    pub permission_denials: String,
    /// Structured output, JSON; empty when none was requested
    #[rune(get)]
    pub structured_output: String,
    /// The id the harness gave the session
    #[rune(get)]
    pub session_id: String,
    #[rune(get)]
    pub uuid: String,
    /// The process's exit code; -1 when it was signaled
    #[rune(get, copy)]
    pub exit_code: i64,
    #[rune(get)]
    pub stderr: String,
}

impl AgentResult {
    fn new(o: AgentOutcome, fallback_stop_reason: &str, exit_code: i32, stderr: String) -> Self {
        let stop_reason = if o.stop_reason.is_empty() {
            fallback_stop_reason.to_string()
        } else {
            o.stop_reason
        };
        AgentResult {
            text: o.text,
            is_error: o.is_error,
            stop_reason,
            turns: o.turns,
            duration_ms: o.duration_ms,
            duration_api_ms: o.duration_api_ms,
            cost_usd: o.cost_usd,
            usage: o.usage,
            model_usage: o.model_usage,
            permission_denials: o.permission_denials,
            structured_output: o.structured_output,
            session_id: o.session_id,
            uuid: o.uuid,
            exit_code: i64::from(exit_code),
            stderr,
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AgentResult {{ exit_code: {}, is_error: {}, stop_reason: {:?}, turns: {}, \
             duration_ms: {}, cost_usd: {}, session_id: {:?}, text: {:?}, stderr: {:?} }}",
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

    /// The fields suited to a note's metadata. `duration` is in
    /// seconds.
    #[rune::function(instance)]
    fn as_metadata(&self) -> Object {
        let mut obj = Object::new();
        let mut put = |k: &str, v: rune::runtime::Value| {
            obj.insert(rune::alloc::String::try_from(k).unwrap(), v)
                .unwrap();
        };
        put("is_error", rune::to_value(self.is_error).unwrap());
        put(
            "stop_reason",
            rune::to_value(self.stop_reason.clone()).unwrap(),
        );
        put("turns", rune::to_value(self.turns).unwrap());
        put(
            "duration",
            rune::to_value(self.duration_ms as f64 / 1000.0).unwrap(),
        );
        put("cost_usd", rune::to_value(self.cost_usd).unwrap());
        put(
            "session_id",
            rune::to_value(self.session_id.clone()).unwrap(),
        );
        put("exit_code", rune::to_value(self.exit_code).unwrap());
        put("stderr", rune::to_value(self.stderr.clone()).unwrap());
        obj
    }
}

/// One item from `Agent::poll`, at content-block granularity.
#[derive(Any, Clone, Debug)]
#[rune(item = ::gage)]
pub enum Event {
    #[rune(constructor)]
    Assistant(#[rune(get)] String),
    #[rune(constructor)]
    Thinking(#[rune(get)] String),
    /// `input` is the tool input as a JSON string
    #[rune(constructor)]
    ToolUse {
        #[rune(get)]
        name: String,
        #[rune(get)]
        input: String,
    },
    #[rune(constructor)]
    ToolResult {
        #[rune(get)]
        id: String,
        #[rune(get)]
        output: String,
    },
    /// A harness system message, JSON-encoded
    #[rune(constructor)]
    System(#[rune(get)] String),
    /// The model finished a turn with this stop reason. The session
    /// may continue with `send`, or has already ended when the harness
    /// had no background work outstanding.
    #[rune(constructor)]
    TurnEnd(#[rune(get)] String),
    /// The session has ended. The last event; a later poll fails
    /// with `AgentError::Stopped`.
    #[rune(constructor)]
    Stop(#[rune(get)] String),
    /// A harness message of a kind not modeled here, JSON-encoded
    #[rune(constructor)]
    Other(#[rune(get)] String),
    /// An output line the driver could not parse
    #[rune(constructor)]
    ParseError(#[rune(get)] String),
}

impl From<AgentEvent> for Event {
    fn from(ev: AgentEvent) -> Self {
        match ev {
            AgentEvent::Assistant(t) => Event::Assistant(t),
            AgentEvent::Thinking(t) => Event::Thinking(t),
            AgentEvent::ToolUse { name, input } => Event::ToolUse { name, input },
            AgentEvent::ToolResult { id, output } => Event::ToolResult { id, output },
            AgentEvent::System(s) => Event::System(s),
            AgentEvent::TurnEnd { outcome, .. } => Event::TurnEnd(outcome.stop_reason),
            AgentEvent::Other(s) => Event::Other(s),
            AgentEvent::ParseError(s) => Event::ParseError(s),
        }
    }
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
