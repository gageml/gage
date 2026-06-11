use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use datafusion::prelude::SessionContext as DfSessionContext;
use gage_claude::project::Project;
use gage_claude::session::SessionInfo;
use gage_db::rusqlite::Connection;
use serde_json as json;
use tokio::sync::mpsc;

use crate::scheduler::WorkerMsg;

/// State shared by all scanners for a single scan run.
///
/// Immutable after run-init. Tasks read this through `current_scan_ctx()`.
#[allow(dead_code)] // scan_id is informational; not yet exposed to scanners.
pub struct RunContext {
    pub scan_id: String,
    /// Selected sessions for this run, in load order.
    pub selected: Arc<[SessionInfo]>,
    /// Sanitized-cwd -> resolved Project, populated only for projects
    /// that resolve to a real on-disk directory.
    pub projects: HashMap<String, Arc<Project>>,
}

/// The target a task is running against.
///
/// Determines what `session()`, `project()`, and the
/// SQL `entry`/`message` views see.
#[derive(Clone)]
pub enum TaskTarget {
    /// `session` context: per-session task.
    Session {
        info: Arc<SessionInfo>,
        project: Option<Arc<Project>>,
    },
    /// `scan` context: one call over the full selected cohort,
    /// consuming notes emitted by upstream tasks.
    Scan,
    /// `project` context: per-project task scoped to the project's
    /// Claude config. The carried `Project` resolves to the project's
    /// cwd on disk; the task examines project config rather than
    /// session activity.
    Project(Arc<Project>),
}

/// Per-task state injected via `tokio::task_local!`.
///
/// Read inside Rune runtime functions via `current_scan_ctx()`. A fresh
/// instance is constructed for every task invocation.
pub struct ScanContext {
    pub scanner_name: String,
    pub params: Option<json::Value>,
    pub run: Arc<RunContext>,
    pub target: TaskTarget,
    pub df_ctx: Option<DfSessionContext>,
    pub db: Arc<Mutex<Connection>>,
    /// Channel from runtime functions (`print`/`println`) back to the
    /// scheduler driver. Workers share this channel for `Started`/
    /// `Completed` signaling.
    pub runtime_tx: mpsc::UnboundedSender<WorkerMsg>,
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
