use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gage_claude::project::Project;
use gage_claude::session::SessionInfo;
use gage_db::rusqlite::Connection;
use gage_query::ScanSessionContext;
use serde_json as json;
use tokio::sync::mpsc;

use crate::RuntimeOutput;
use crate::dispatcher::ToolDispatcher;
use std::sync::OnceLock;

/// State shared by all scanners for a single scan run.
///
/// Immutable after run-init. Tasks read this through `current_scan_ctx()`.
pub struct RunContext {
    pub scan_id: String,
    /// Selected sessions for this run, in load order.
    pub selected: Arc<[SessionInfo]>,
    /// Sanitized-cwd -> resolved Project, populated only for projects
    /// that resolve to a real on-disk directory.
    #[allow(dead_code)] // retained for future project-scoped queries.
    pub projects: HashMap<String, Arc<Project>>,
    /// One DataFusion context for the whole run. Registers the `entry`
    /// and `message` tables backed by a `Lookup` source over `selected`
    /// and shares a `SessionCache`, so per-session derives amortize
    /// across every `s.messages()` / `s.entries()` / `query(...)` call
    /// in any scanner. Exposes `cached_session_count()` for progress.
    pub scan_ctx: Arc<ScanSessionContext>,
    /// In-process HTTP MCP host shared across every `call_agent`
    /// invocation in this run. `None` in test contexts that don't
    /// exercise `call_agent`; `call_agent` errors when this is unset.
    pub mcp_host: Option<Arc<gage_mcp::McpHost>>,
    /// Tool dispatch server. Initialized after the `RunContext`'s Arc
    /// is created so the dispatcher can hold a `Weak<RunContext>`
    /// without forming a strong cycle. Set once via [`OnceLock::set`].
    pub dispatcher: OnceLock<Arc<ToolDispatcher>>,
    /// Run-wide cap on concurrent `call_agent` invocations. Every
    /// `call_agent(...).await` acquires one permit before spawning the
    /// claude child and holds it for the lifetime of the resulting
    /// [`crate::agent::Agent`]. Configured via the CLI's
    /// `--agent-jobs` flag.
    pub agent_pool: Arc<tokio::sync::Semaphore>,
    /// Invalidate prior validation state. When set, the `split_valid`
    /// / `split_valid_range` / `notes.split_valid` / `files_valid`
    /// entry points delete the recorded rows for the keys and refs
    /// they present and classify every item as new, forcing the run's
    /// tasks to redo their work. Set from the CLI's `--invalidate`
    /// flag.
    pub invalidate: bool,
    /// Run-wide agent-facility fault, set at most once. Installed when
    /// a condition makes every agent call pointless for the rest of the
    /// run (claude not logged in). Once set, every Rune-visible agent
    /// entry point raises the message as an uncatchable VM panic
    /// without spawning or contacting a claude child — see
    /// `agent::with_fault_barrier`.
    pub agent_fault: OnceLock<String>,
}

/// Per-task state injected via `tokio::task_local!`.
///
/// Read inside Rune runtime functions via `current_scan_ctx()`. A fresh
/// instance is constructed for every task invocation.
pub struct ScanContext {
    pub scanner_name: String,
    /// Name of the running task function. Identifies the sender on
    /// progress messages.
    pub task_name: String,
    pub params: Option<json::Value>,
    pub run: Arc<RunContext>,
    pub db: Arc<Mutex<Connection>>,
    /// Channel from Rune `print`/`println` calls back to the scan
    /// orchestrator. The receiver lives in the scheduler.
    pub runtime_tx: mpsc::UnboundedSender<RuntimeOutput>,
    /// Scanner's compiled Rune runtime context — needed by
    /// `call_agent` to spawn a fresh Vm per scanner-tool callback
    /// (see `.tools(...)` dispatch in `agent.rs`).
    pub rt: rune::sync::Arc<rune::runtime::RuntimeContext>,
    pub unit: rune::sync::Arc<rune::runtime::Unit>,
    pub sources: Arc<rune::Sources>,
    /// Fault the runtime detected while this task ran (claude not
    /// logged in). The runtime records the message here and then
    /// aborts the task's VM; the abort is only the unwind vehicle, so
    /// the dispatcher reports this message as the task's failure and
    /// discards the VM error's own rendering. `None` when the task
    /// ended without a detected fault.
    pub task_fault: Mutex<Option<String>>,
}

tokio::task_local! {
    pub static SCAN_CTX: Arc<ScanContext>;
}

/// Return the current task's `ScanContext`. Panics if called outside a
/// task scope (programmer error in Rust glue — never reachable from a
/// scanner).
pub fn current_scan_ctx() -> Arc<ScanContext> {
    SCAN_CTX.with(|c| c.clone())
}

/// Recorded fault on a scanner. Once set, all subsequent tasks for the
/// scanner are skipped.
#[derive(Debug, Clone)]
#[allow(dead_code)] // surfaced via tracing logs.
pub struct Fault {
    pub task_name: String,
    pub message: String,
}

/// Compilation artifacts + per-scanner mutable bits that survive the
/// run. The `Vm` is not stored here — a fresh one is built per task.
#[allow(dead_code)] // embed_key/source_path/source are kept for
// future scan_progress / error reporting work
pub struct ScannerSlot {
    pub name: String,
    pub embed_key: String,
    pub source_path: PathBuf,
    pub source: String,
    pub params: Option<json::Value>,
    pub rt: rune::sync::Arc<rune::runtime::RuntimeContext>,
    pub unit: rune::sync::Arc<rune::runtime::Unit>,
    pub sources: Arc<rune::Sources>,
    /// Process-wide shared connection. Every ScannerSlot in a run holds
    /// a clone of the same Arc, and every task read and write goes
    /// through this Mutex. Combined with the scheduler's DAG gating,
    /// this is what guarantees a downstream task sees every note its
    /// upstream tasks wrote. Do not give a slot its own connection —
    /// separate WAL connections take snapshot reads and break that
    /// guarantee.
    pub db: Arc<Mutex<Connection>>,
    pub fault: Mutex<Option<Fault>>,
}
