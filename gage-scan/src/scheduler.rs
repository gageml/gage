//! Task scheduler: builds a single DAG over scanner tasks and dispatches
//! them to a worker pool.
//!
//! - One DAG covering all tasks across all context types
//!   (`session` / `project` / `scan`). There are no phase barriers.
//! - Nodes are `Task` values (immutable plan units).
//! - Edges come from `notes.wants`/`notes.writes` declarations: a task's
//!   `notes.wants` lists *note names* it consumes; the planner
//!   reverse-looks-up every task in the same scanner whose `notes.writes`
//!   contains that note name and adds an edge from each producer instance
//!   to the consumer instance whose targets are compatible.
//! - Unsatisfied `notes.wants` (no task in the scanner writes the note) is a
//!   planner *warning*, not an error. The task still runs.
//! - Cycle detection runs at plan time over the full graph.
//! - Worker pool: N tokio tasks pulling from an unbounded ready queue.
//!   Per-scanner concurrency is unrestricted — each task builds a
//!   fresh [`rune::Vm`] from the scanner's shared compilation
//!   artifacts.
//!
//! Note: scanner module-level state is NOT preserved across tasks.
//! Tasks are independent invocations and communicate via notes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use datafusion::prelude::SessionContext as DfSessionContext;
use gage_claude::project::Project;
use gage_claude::session::SessionInfo;
use gage_query::tables::{EntryTable, MessageTable};
use petgraph::Graph;
use petgraph::algo::tarjan_scc;
use petgraph::graph::NodeIndex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::event::{RunStatus, RunSummary, ScanEvent, TargetLabel, TaskRef, WorkerStatus};
use crate::runtime::state::{Fault, RunContext, SCAN_CTX, ScanContext, ScannerSlot, TaskTarget};
use crate::scanner::{TaskContext, TaskDef};

/// One planned invocation: a function call from a scanner against a
/// specific target. Immutable once the planner finishes.
pub(crate) struct Task {
    pub scanner_idx: usize,
    pub task_name: String,
    pub target: TaskTarget,
}

/// Result of dispatching one task.
#[derive(Debug)]
pub(crate) enum TaskResult {
    Ok,
    /// Task function returned `Err(value)`; the string is the typed
    /// Error's Display (see `render_task_error`).
    Error(String),
    /// VM-level failure (panic, missing function, type error, etc.).
    /// The string is pre-rendered with a source frame via codespan.
    VmError(String),
    /// Task was skipped because its scanner already faulted.
    SkippedByFault,
}

/// Channel message — emitted by worker tasks (Started, Completed) and
/// Rune runtime functions (Print, Println). The scheduler driver
/// consumes these to drive the DAG and to emit public events.
#[derive(Debug)]
pub(crate) enum WorkerMsg {
    Started {
        worker_id: usize,
        task_idx: usize,
    },
    Completed {
        worker_id: usize,
        task_idx: usize,
        outcome: TaskResult,
    },
    Print {
        s: String,
    },
    Println {
        s: String,
    },
}

pub(crate) struct Plan {
    pub tasks: Vec<Task>,
    /// Adjacency: tasks[i] downstream task indices.
    pub downstream: Vec<Vec<usize>>,
    /// Initial in-degree per task.
    pub deps: Vec<u32>,
    /// Non-fatal warnings produced during planning (e.g. an unsatisfied
    /// `wants` note). Surfaced through the event sink before the run
    /// starts.
    pub warnings: Vec<PlanWarning>,
}

pub(crate) struct PlanError {
    pub scanner: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PlanWarning {
    pub scanner: String,
    pub task: String,
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.scanner, self.message)
    }
}

/// Build a single DAG over all tasks across all context types.
///
/// Targets are expanded by context:
/// - [`TaskContext::Project`]: one task per resolved project per scanner task.
/// - [`TaskContext::Session`]: one task per selected session per scanner task.
/// - [`TaskContext::Scan`]: one task per scanner task.
#[allow(clippy::indexing_slicing)]
pub(crate) fn plan(
    scanners: &[ScannerSlot],
    scanner_tasks: &[HashMap<String, TaskDef>],
    run: &Arc<RunContext>,
) -> Result<Plan, PlanError> {
    let mut graph = Graph::<usize, ()>::new();
    let mut tasks: Vec<Task> = Vec::new();
    let mut warnings: Vec<PlanWarning> = Vec::new();
    // (scanner_idx, task_name, target_key) -> node index
    let mut node_index: HashMap<(usize, String, TargetKey), NodeIndex> = HashMap::new();

    // Walk tasks in (scanner_name, task_name) ascending order so
    // dispatch and UI display are deterministic. Topological order
    // from `notes.wants` is still honored by the dep counts wired below;
    // this sort is the tie-breaker for tasks that are ready
    // simultaneously.
    let mut planned: Vec<(usize, &String, &TaskDef)> = Vec::new();
    for (scanner_idx, defs) in scanner_tasks.iter().enumerate() {
        for (task_name, def) in defs {
            planned.push((scanner_idx, task_name, def));
        }
    }
    planned.sort_by(|a, b| {
        scanners[a.0]
            .name
            .cmp(&scanners[b.0].name)
            .then_with(|| a.1.cmp(b.1))
    });

    for &(scanner_idx, task_name, def) in &planned {
        for (target, target_key) in expand_targets(def.context, run) {
            let task_idx = tasks.len();
            tasks.push(Task {
                scanner_idx,
                task_name: task_name.clone(),
                target,
            });
            let node = graph.add_node(task_idx);
            node_index.insert((scanner_idx, task_name.clone(), target_key), node);
        }
    }

    // Build per-scanner `note_name -> [task_name]` index from `notes.writes`.
    let mut writes_index: Vec<HashMap<String, Vec<String>>> =
        vec![HashMap::new(); scanner_tasks.len()];
    for (scanner_idx, defs) in scanner_tasks.iter().enumerate() {
        for (task_name, def) in defs {
            for note_name in def.notes.writes.keys() {
                writes_index[scanner_idx]
                    .entry(note_name.clone())
                    .or_default()
                    .push(task_name.clone());
            }
        }
    }

    // Wire dependencies.
    //
    // Each name in `notes.wants` is a *note name*. For every wanted note,
    // find all tasks (within the same scanner) whose `notes.writes` includes
    // that name. For every (producer instance, consumer instance) pair
    // whose targets are compatible, add an edge.
    //
    // An unsatisfied `notes.wants` (no task in the scanner writes the note)
    // is recorded as a warning, not an error — the consumer still
    // runs.
    for &(scanner_idx, task_name, def) in &planned {
        for want in &def.notes.wants {
            let producers = writes_index[scanner_idx].get(want);
            let Some(producers) = producers else {
                warnings.push(PlanWarning {
                    scanner: scanners[scanner_idx].name.clone(),
                    task: task_name.clone(),
                    message: format!("wants note '{want}' but no task in this scanner writes it"),
                });
                continue;
            };
            for producer in producers {
                if producer == task_name {
                    // A task wanting a note it writes itself is a no-op
                    // dependency — would create a self-loop. Skip.
                    continue;
                }
                let producer_def = &scanner_tasks[scanner_idx][producer];
                for (_, producer_key) in expand_targets(producer_def.context, run) {
                    for (_, consumer_key) in expand_targets(def.context, run) {
                        if !targets_compatible(&producer_key, &consumer_key, run) {
                            continue;
                        }
                        let from = *node_index
                            .get(&(scanner_idx, producer.clone(), producer_key.clone()))
                            .unwrap();
                        let to = *node_index
                            .get(&(scanner_idx, task_name.clone(), consumer_key.clone()))
                            .unwrap();
                        if from == to {
                            continue;
                        }
                        graph.add_edge(from, to, ());
                    }
                }
            }
        }
    }

    // Cycle detection via SCC over the full graph.
    let sccs = tarjan_scc(&graph);
    for scc in &sccs {
        if scc.len() > 1 {
            let names: Vec<String> = scc
                .iter()
                .map(|n| {
                    let t = &tasks[*graph.node_weight(*n).unwrap()];
                    format!("{}::{}", scanners[t.scanner_idx].name, t.task_name)
                })
                .collect();
            return Err(PlanError {
                scanner: scanners[tasks[*graph.node_weight(scc[0]).unwrap()].scanner_idx]
                    .name
                    .clone(),
                message: format!("cycle in task dependencies: {}", names.join(" -> ")),
            });
        }
    }

    // Build adjacency arrays indexed by task index.
    let mut downstream = vec![Vec::new(); tasks.len()];
    let mut deps = vec![0u32; tasks.len()];
    for edge in graph.edge_indices() {
        let (from, to) = graph.edge_endpoints(edge).unwrap();
        let from_task = *graph.node_weight(from).unwrap();
        let to_task = *graph.node_weight(to).unwrap();
        downstream[from_task].push(to_task);
        deps[to_task] += 1;
    }
    for d in &mut downstream {
        d.sort_unstable();
        d.dedup();
    }

    Ok(Plan {
        tasks,
        downstream,
        deps,
        warnings,
    })
}

/// Identifier used to group tasks by their target for dependency
/// resolution.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum TargetKey {
    Session(String),
    Project(String),
    Scan,
}

/// Two targets are compatible (i.e. an edge between tasks at these
/// targets should be created) when:
/// - they refer to the same session, or
/// - they refer to the same project, or
/// - one is `Scan` (the singleton scan target sees all evidence), or
/// - one is `Session` for a project and the other is that project's
///   `Project` target.
fn targets_compatible(u: &TargetKey, d: &TargetKey, run: &Arc<RunContext>) -> bool {
    match (u, d) {
        (TargetKey::Session(a), TargetKey::Session(b)) => a == b,
        (TargetKey::Project(a), TargetKey::Project(b)) => a == b,
        (TargetKey::Scan, _) | (_, TargetKey::Scan) => true,
        (TargetKey::Session(sid), TargetKey::Project(pname))
        | (TargetKey::Project(pname), TargetKey::Session(sid)) => run
            .selected
            .iter()
            .find(|s| s.id == *sid)
            .is_some_and(|s| s.project_name() == *pname),
    }
}

fn expand_targets(context: TaskContext, run: &Arc<RunContext>) -> Vec<(TaskTarget, TargetKey)> {
    match context {
        TaskContext::Session => run
            .selected
            .iter()
            .map(|s| {
                let info: Arc<SessionInfo> = Arc::new(s.clone());
                let project = run.projects.get(&*s.project_name()).cloned();
                let key = TargetKey::Session(info.id.clone());
                (TaskTarget::Session { info, project }, key)
            })
            .collect(),
        TaskContext::Project => {
            let mut seen: HashMap<String, Arc<Project>> = HashMap::new();
            for s in run.selected.iter() {
                if let Some(p) = run.projects.get(&*s.project_name()) {
                    seen.entry(p.path.to_string_lossy().into_owned())
                        .or_insert_with(|| p.clone());
                }
            }
            let mut out: Vec<_> = seen
                .into_iter()
                .map(|(k, p)| (TaskTarget::Project(p), TargetKey::Project(k)))
                .collect();
            out.sort_by(|a, b| match (&a.1, &b.1) {
                (TargetKey::Project(a), TargetKey::Project(b)) => a.cmp(b),
                _ => std::cmp::Ordering::Equal,
            });
            out
        }
        TaskContext::Scan => vec![(TaskTarget::Scan, TargetKey::Scan)],
    }
}

/// Run a single plan, dispatching tasks to a pool of `jobs` workers.
pub(crate) async fn run_plan(
    plan: Plan,
    scanners: Arc<Vec<ScannerSlot>>,
    run: Arc<RunContext>,
    jobs: usize,
    cancel: CancellationToken,
    mut on_event: impl FnMut(ScanEvent) + Send,
) -> Result<RunSummary, RunError> {
    let jobs = jobs.max(1);
    let plan_total = plan.tasks.len();

    // Surface planner warnings before any task runs.
    for w in &plan.warnings {
        on_event(ScanEvent::Warning {
            scanner: w.scanner.clone(),
            task: w.task.clone(),
            message: w.message.clone(),
        });
    }

    // Authoritative live state. Mutated only on the driver and shipped
    // as cloned snapshots through the event callback.
    let mut status = RunStatus {
        scan_id: run.scan_id.clone(),
        total: plan_total,
        progress: 0,
        workers: (0..jobs)
            .map(|id| WorkerStatus { id, current: None })
            .collect(),
    };
    let mut accounting = RunAccounting {
        completed: 0,
        failed: 0,
        skipped: 0,
    };

    on_event(ScanEvent::Status(status.clone()));

    debug!(tasks = plan.tasks.len(), "scheduling");
    run_tasks(
        plan,
        &scanners,
        &run,
        jobs,
        &cancel,
        &mut status,
        &mut accounting,
        &mut on_event,
    )
    .await?;

    Ok(RunSummary {
        scan_id: status.scan_id,
        total: plan_total,
        completed: accounting.completed,
        failed: accounting.failed,
        skipped: accounting.skipped,
    })
}

struct RunAccounting {
    completed: usize,
    failed: usize,
    skipped: usize,
}

pub enum RunError {
    Channel,
    Canceled,
}

#[allow(clippy::indexing_slicing, clippy::too_many_arguments)]
async fn run_tasks(
    plan: Plan,
    scanners: &Arc<Vec<ScannerSlot>>,
    run: &Arc<RunContext>,
    jobs: usize,
    cancel: &CancellationToken,
    status: &mut RunStatus,
    accounting: &mut RunAccounting,
    on_event: &mut (impl FnMut(ScanEvent) + Send),
) -> Result<(), RunError> {
    let task_count = plan.tasks.len();
    let tasks = Arc::new(plan.tasks);
    let downstream = Arc::new(plan.downstream);
    let deps_remaining: Arc<Vec<AtomicU32>> =
        Arc::new(plan.deps.iter().map(|d| AtomicU32::new(*d)).collect());

    let (ready_tx, ready_rx) = mpsc::unbounded_channel::<usize>();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<WorkerMsg>();

    // Seed initial ready set.
    for (i, d) in deps_remaining.iter().enumerate() {
        if d.load(Ordering::SeqCst) == 0 {
            ready_tx.send(i).expect("ready channel open");
        }
    }

    let ready_rx = Arc::new(tokio::sync::Mutex::new(ready_rx));
    let mut worker_handles = Vec::new();
    for worker_id in 0..jobs {
        let scanners = scanners.clone();
        let run = run.clone();
        let tasks = tasks.clone();
        let ready_rx = ready_rx.clone();
        let msg_tx = msg_tx.clone();
        worker_handles.push(tokio::spawn(async move {
            loop {
                let task_idx = {
                    let mut rx = ready_rx.lock().await;
                    rx.recv().await
                };
                let Some(task_idx) = task_idx else { break };
                let task = &tasks[task_idx];
                if msg_tx
                    .send(WorkerMsg::Started {
                        worker_id,
                        task_idx,
                    })
                    .is_err()
                {
                    break;
                }
                let outcome = dispatch_task(task, &scanners, &run, &msg_tx).await;
                if msg_tx
                    .send(WorkerMsg::Completed {
                        worker_id,
                        task_idx,
                        outcome,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    drop(msg_tx);

    let mut completed = 0usize;
    let mut canceled = false;
    while completed < task_count {
        let msg = tokio::select! {
            biased;
            _ = cancel.cancelled(), if !canceled => {
                canceled = true;
                for h in &worker_handles {
                    h.abort();
                }
                continue;
            }
            msg = msg_rx.recv() => msg,
        };
        let Some(msg) = msg else {
            if canceled {
                break;
            }
            return Err(RunError::Channel);
        };
        match msg {
            WorkerMsg::Started {
                worker_id,
                task_idx,
            } => {
                let task = &tasks[task_idx];
                let slot = &scanners[task.scanner_idx];
                status.workers[worker_id].current = Some(TaskRef {
                    scanner: slot.name.clone(),
                    task: task.task_name.clone(),
                    target: target_label(&task.target),
                });
                on_event(ScanEvent::Status(status.clone()));
            }
            WorkerMsg::Completed {
                worker_id,
                task_idx,
                outcome,
            } => {
                let task = &tasks[task_idx];
                status.workers[worker_id].current = None;
                match &outcome {
                    TaskResult::Ok => {
                        accounting.completed += 1;
                        status.progress += 1;
                    }
                    TaskResult::Error(msg) | TaskResult::VmError(msg) => {
                        accounting.failed += 1;
                        status.progress += 1;
                        on_event(ScanEvent::TaskFailed {
                            scanner: scanners[task.scanner_idx].name.clone(),
                            task: task.task_name.clone(),
                            target: target_label(&task.target),
                            message: msg.clone(),
                        });
                    }
                    TaskResult::SkippedByFault => {
                        accounting.skipped += 1;
                        // Fault-skips remove work from the pipeline
                        // rather than counting toward progress.
                        status.total = status.total.saturating_sub(1);
                    }
                }
                on_event(ScanEvent::Status(status.clone()));
                completed += 1;
                for &down in &downstream[task_idx] {
                    let prev = deps_remaining[down].fetch_sub(1, Ordering::SeqCst);
                    if prev == 1 {
                        ready_tx.send(down).expect("ready channel open");
                    }
                }
            }
            WorkerMsg::Print { s } => {
                on_event(ScanEvent::Print { s });
            }
            WorkerMsg::Println { s } => {
                on_event(ScanEvent::Println { s });
            }
        }
    }
    drop(ready_tx);
    for h in worker_handles {
        match h.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() || e.is_panic() => {}
            Err(e) => panic!("worker join: {e}"),
        }
    }

    if canceled {
        return Err(RunError::Canceled);
    }
    Ok(())
}

fn target_label(target: &TaskTarget) -> TargetLabel {
    match target {
        TaskTarget::Session { info, .. } => TargetLabel::Session(info.id.clone()),
        TaskTarget::Project(p) => TargetLabel::Project(p.path.to_string_lossy().into_owned()),
        TaskTarget::Scan => TargetLabel::Scan,
    }
}

#[allow(clippy::indexing_slicing)]
async fn dispatch_task(
    task: &Task,
    scanners: &[ScannerSlot],
    run: &Arc<RunContext>,
    msg_tx: &mpsc::UnboundedSender<WorkerMsg>,
) -> TaskResult {
    let slot = &scanners[task.scanner_idx];

    if slot.fault.lock().unwrap().is_some() {
        return TaskResult::SkippedByFault;
    }

    // Session tasks see the full session content on every scan. There
    // is no skipping or line pruning: idempotency is enforced by note
    // and issue dedup, not by task validation.
    let df_ctx = match &task.target {
        TaskTarget::Session { info, .. } => Some(build_session_ctx(&info.id, &info.src)),
        _ => None,
    };

    let ctx = Arc::new(ScanContext {
        scanner_name: slot.name.clone(),
        params: slot.params.clone(),
        run: run.clone(),
        target: task.target.clone(),
        df_ctx,
        db: slot.db.clone(),
        runtime_tx: msg_tx.clone(),
    });

    let task_name = task.task_name.clone();
    let rt = slot.rt.clone();
    let unit = slot.unit.clone();
    let sources = slot.sources.clone();
    let label_task = task_name.clone();
    let outcome = SCAN_CTX
        .scope(ctx, async move {
            let vm = rune::Vm::new(rt, unit);
            let execution = match vm.send_execute([task_name.as_str()], ()) {
                Ok(e) => e,
                Err(e) => return TaskResult::VmError(render_vm_err(&e, &sources)),
            };
            match execution.complete().await {
                Ok(val) => match rune::from_value::<
                    Result<rune::runtime::Value, rune::runtime::Value>,
                >(val)
                {
                    Ok(Err(err)) if crate::runtime::ignore::is_ignore(&err) => TaskResult::Ok,
                    Ok(Err(err)) => TaskResult::Error(render_task_error(err)),
                    _ => TaskResult::Ok,
                },
                Err(e) => TaskResult::VmError(render_vm_err(&e, &sources)),
            }
        })
        .await;

    if let TaskResult::Error(msg) | TaskResult::VmError(msg) = &outcome {
        let mut fault = slot.fault.lock().unwrap();
        if fault.is_none() {
            *fault = Some(Fault {
                task_name: label_task,
                message: msg.clone(),
            });
        }
    }

    outcome
}

fn render_vm_err(e: &rune::runtime::VmError, sources: &rune::Sources) -> String {
    crate::runner::render_vm_error(e, sources, &e.to_string())
}

// Render an `Err` value a task returned to a human string. A task that
// returns an unexpected `Err` is a programming error, so we want
// debug-level detail. Must not Debug-format the raw Rune value: that
// dispatches the DEBUG_FMT protocol via the interface environment, which
// is not set here (we run after the VM has finished), turning every task
// error into an opaque "Missing interface environment". Downcast to the
// typed Error and use its Rust `Debug` instead — that is plain Rust
// formatting, not the Rune protocol, so it is safe post-VM.
fn render_task_error(err: rune::runtime::Value) -> String {
    if let Ok(e) = rune::from_value::<crate::runtime::error::Error>(err.clone()) {
        format!("{e:?}")
    } else if let Ok(s) = rune::from_value::<String>(err.clone()) {
        s
    } else {
        format!(
            "task returned a non-error value of type `{}`",
            err.type_info()
        )
    }
}

fn build_session_ctx(session_id: &str, path: &Path) -> DfSessionContext {
    let ctx = DfSessionContext::new();
    gage_query::register_udfs(&ctx);
    let entry = EntryTable::with_session(session_id.to_string(), path.to_path_buf());
    let message = MessageTable::with_session(session_id.to_string(), path.to_path_buf());
    ctx.register_table("entry", std::sync::Arc::new(entry))
        .expect("register entry table");
    ctx.register_table("message", std::sync::Arc::new(message))
        .expect("register message table");
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::error::Error;

    // Guards the off-VM rendering path: a task's Err must render via the
    // typed Error's Rust Debug, never the Rune DEBUG_FMT protocol (which
    // would fail with "Missing interface environment" here).
    #[test]
    fn render_task_error_renders_typed_error() {
        let err = rune::to_value(Error::Args("bad field".to_string())).unwrap();
        assert_eq!(render_task_error(err), r#"Args("bad field")"#);
    }

    #[test]
    fn render_task_error_renders_plain_string() {
        let err = rune::to_value("boom".to_string()).unwrap();
        assert_eq!(render_task_error(err), "boom");
    }
}
