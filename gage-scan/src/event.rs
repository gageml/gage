//! Events emitted by the scanner runner during a run.
//!
//! The runner is a pure event emitter. Consumers (CLI, MCP, tests)
//! implement an event sink. Each `Status` event is a self-contained
//! snapshot — never reassemble state from partial deltas.

/// The task a worker is currently running, with any task-reported
/// progress.
#[derive(Debug, Clone)]
pub struct ActiveTask {
    pub scanner: String,
    pub task: String,
    /// Latest task-reported `(pos, total)`; `None` means the task has
    /// not reported and renders as indeterminate.
    pub progress: Option<(u64, u64)>,
    /// Agents queued waiting on the run-wide agent pool.
    pub agents_waiting: u64,
    /// Agents holding a pool permit (running).
    pub agents_active: u64,
    /// Completed spells of pool blockage: time spent with at least one
    /// agent waiting and none active. Excludes the current spell; use
    /// [`ActiveTask::blocked_total`] for display.
    pub agent_blocked: std::time::Duration,
    /// Start of the current fully-blocked spell; `None` when the task
    /// is not currently blocked on the pool.
    pub blocked_since: Option<std::time::Instant>,
}

impl ActiveTask {
    /// The task is blocked on the agent pool right now: at least one
    /// agent is waiting for a permit and none holds one.
    pub fn pool_blocked(&self) -> bool {
        self.agents_active == 0 && self.agents_waiting > 0
    }

    /// Total time blocked on the agent pool, including the current
    /// spell.
    pub fn blocked_total(&self) -> std::time::Duration {
        self.agent_blocked + self.blocked_since.map(|s| s.elapsed()).unwrap_or_default()
    }
}

/// One worker slot. `current` reflects what that worker is doing right
/// now; `None` means the worker is idle.
#[derive(Debug, Clone)]
pub struct WorkerStatus {
    pub id: usize,
    pub current: Option<ActiveTask>,
}

/// Full live state of a scan run. Self-contained — every emission is a
/// complete picture so consumers never reassemble.
#[derive(Debug, Clone)]
pub struct RunStatus {
    pub scan_id: String,
    /// Tasks that will be processed. Decreases when a scanner faults
    /// and its remaining tasks are removed from the pipeline. Use with
    /// `progress` directly as bar length/position.
    pub total: usize,
    /// Completed + failed. Drives the bar position.
    pub progress: usize,
    pub workers: Vec<WorkerStatus>,
}

/// End-of-run accounting. Returned from `run()`. Distinct from
/// `RunStatus` because the final report wants the skipped count broken
/// out separately.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunSummary {
    pub scan_id: String,
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// The run was canceled before completing; the counts cover only
    /// what ran before the cancellation.
    pub canceled: bool,
}

#[derive(Debug)]
pub enum ScanEvent {
    /// Self-contained progress snapshot.
    Status(RunStatus),
    /// Bytes from a scanner `print(...)` call, verbatim.
    Print { s: String },
    /// Bytes from a scanner `println(...)` call, verbatim (no trailing newline).
    Println { s: String },
    /// A task returned an Err or panicked. The UI is responsible for
    /// rendering this above any active progress bars.
    TaskFailed {
        scanner: String,
        task: String,
        message: String,
    },
    /// A non-fatal planner warning (e.g. an unsatisfied `wants` note).
    /// The task still runs; the wanted note simply isn't available.
    Warning {
        scanner: String,
        task: String,
        message: String,
    },
}
