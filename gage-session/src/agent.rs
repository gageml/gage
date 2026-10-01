//! Agent running through a driver.
//!
//! A driver that can run an agent takes an [`AgentSpec`] and returns
//! an [`AgentSession`]: a live handle over the harness process that
//! yields [`AgentEvent`]s as the agent works, accepts user messages
//! and interrupts, and, once the process has exited, presents the
//! transcript as a [`NativeSession`] for storing. The vocabulary here
//! is what every harness has: text, thinking, tool calls and results,
//! a turn ending with a result, and the process ending.

use std::io;
use std::time::Duration;

use async_trait::async_trait;

use crate::{DriverError, NativeSession};

/// What to run. The driver resolves `model` and applies the system
/// prompt in its harness's terms.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// The initial user message
    pub prompt: String,
    /// A model alias (`small`, `medium`, `large`) or a name the driver
    /// accepts. `None` is the driver's default.
    pub model: Option<String>,
    pub system_prompt: SystemPrompt,
    /// Text appended to the harness's default system prompt. Implies
    /// [`SystemPrompt::Default`].
    pub system_prompt_append: Option<String>,
    pub max_turns: Option<u32>,
    /// How long to wait for the process to exit once the session is
    /// being ended. The driver's default applies when `None`.
    pub timeout: Option<Duration>,
    /// The project name recorded on the stored transcript, when the
    /// caller gives one. The harness's own notion of project for the
    /// run is otherwise recorded.
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SystemPrompt {
    /// No system prompt at all
    #[default]
    Empty,
    /// The harness's own default system prompt
    Default,
    /// A replacement system prompt
    Custom(String),
}

/// One thing the agent did, at content-block granularity: one
/// assistant message carrying text, thinking, and a tool call yields
/// three events.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Assistant(String),
    Thinking(String),
    /// `input` is the tool input as a JSON string
    ToolUse {
        name: String,
        input: String,
    },
    /// `output` is the result's text, parts joined
    ToolResult {
        id: String,
        output: String,
    },
    /// A harness system message, JSON-encoded
    System(String),
    /// The model finished a turn. The session is still alive: the
    /// caller may send another message or end it. `idle` is true when
    /// the harness has no background work outstanding, so this turn
    /// end is the session's natural end.
    TurnEnd {
        outcome: Box<AgentOutcome>,
        idle: bool,
    },
    /// A harness message of a kind not modeled here, JSON-encoded
    Other(String),
    /// An output line the driver could not parse
    ParseError(String),
}

/// What a turn produced, as the harness reports it. Fields the harness
/// does not report are empty or zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOutcome {
    /// The final assistant text
    pub text: String,
    pub is_error: bool,
    /// `end_turn`, `max_tokens`, and the like
    pub stop_reason: String,
    pub turns: i64,
    pub duration_ms: i64,
    pub duration_api_ms: i64,
    pub cost_usd: f64,
    /// Token counts, JSON
    pub usage: String,
    /// Per-model token and cost breakdown, JSON
    pub model_usage: String,
    /// Blocked tool calls, JSON
    pub permission_denials: String,
    /// Structured output, JSON; empty when none was requested
    pub structured_output: String,
    /// The id the harness gave the session
    pub session_id: String,
    /// The harness's id for this result
    pub uuid: String,
    /// The harness's result message, verbatim
    pub raw: String,
}

/// A running agent. Events come from [`next_event`](Self::next_event)
/// until the harness closes its output; the caller then waits for the
/// process, takes the transcript for storing, and cleans up.
#[async_trait]
pub trait AgentSession: Send {
    /// The next event, or `None` once the harness has closed its
    /// output.
    async fn next_event(&mut self) -> Option<AgentEvent>;

    /// Queue a user message after the current turn.
    async fn send(&mut self, text: &str) -> io::Result<()>;

    /// Interrupt the current turn.
    async fn interrupt(&mut self) -> io::Result<()>;

    /// Close the harness's input. The harness ends its session when
    /// it has nothing queued.
    fn close_input(&mut self);

    /// Wait for the process to exit, within the spec's timeout.
    /// Returns the exit code, or -1 when the process was signaled.
    async fn wait_exit(&mut self) -> io::Result<i32>;

    /// Terminate the process: a polite signal, then a forced one
    /// after `grace`.
    async fn kill(&mut self, grace: Duration) -> io::Result<()>;

    /// The process's stderr, read to its end. Call after exit.
    async fn take_stderr(&mut self) -> io::Result<Vec<u8>>;

    /// The transcript the harness wrote, as a native session the
    /// store can add. `None` when it wrote none. Call after exit.
    fn transcript(&mut self) -> Result<Option<Box<dyn NativeSession + Send>>, DriverError>;

    /// Remove the harness's working files for this run. Call after
    /// the transcript has been stored.
    fn cleanup(&mut self) -> Result<(), DriverError>;
}
