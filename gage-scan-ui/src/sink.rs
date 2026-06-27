//! Progress sink interface — the contract every prototype UI implements.
//!
//! The runner emits `ScanEvent`s; the harness adapts them into the
//! richer shape a UI wants (per-task elapsed, activity counter,
//! aggregated log). UIs consume `UiEvent`s.

use gage_scan::event::RunStatus;

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// New overall snapshot — total tasks, completed count, current
    /// per-worker assignment.
    Status(RunStatus),
    /// One scanner log line (already trimmed of trailing newline).
    Log(String),
    /// Per-task non-fatal warning.
    Warning {
        scanner: String,
        task: String,
        message: String,
    },
    /// Per-task failure with multi-line detail.
    Failed {
        scanner: String,
        task: String,
        message: String,
    },
    /// Stdout byte count delta for activity tracking. Until the runner
    /// emits real token/turn events from the agent process, this is the
    /// only "something is happening" signal we have for long judge
    /// tasks beyond the spinner.
    Bytes(u64),
    /// Final tick — shutdown.
    Finished,
}
