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
