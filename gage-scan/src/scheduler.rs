//! Task scheduler: builds a single DAG over scanner tasks and dispatches
//! them to a worker pool.
//!
//! - One DAG covering every (scanner, task) declaration. There are no
//!   phase barriers; tasks declare their inter-task dependencies through
//!   `notes.wants` / `notes.writes` and `issues.wants` / `issues.writes`.
//! - Nodes are `Task` values (immutable plan units): one per declared
//!   scanner task, dispatched once per run.
//! - Edges come from `wants`/`writes` per item kind: a task's `wants`
//!   lists `*`-glob patterns over item names it consumes; the planner
//!   matches each pattern against every task's `writes` names across the
//!   full plan (`glob_match`) and adds an edge per producer. Notes and
//!   issues are matched independently — a note pattern never matches an
//!   issue name.
//! - An unsatisfied `wants` (no task in the plan writes a matching item)
//!   is a planner *warning*, not an error. The task still runs.
//! - Cycle detection runs at plan time over the full graph.
//! - Worker pool: N tokio tasks pulling from an unbounded ready queue.
//!   Per-scanner concurrency is unrestricted — each task builds a
//!   fresh [`rune::Vm`] from the scanner's shared compilation
//!   artifacts.
//!
//! Note: scanner module-level state is NOT preserved across tasks.
//! Tasks are independent invocations and communicate via notes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use petgraph::Graph;
use petgraph::algo::tarjan_scc;
use petgraph::graph::NodeIndex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::event::{RunStatus, RunSummary, ScanEvent, TaskRef, WorkerStatus};
use gage_core::glob::glob_match;
use gage_db::scan::{TaskOutcome, TaskStatus};
use gage_registry::scanner::TaskDef;
use gage_runtime::state::{Fault, RunContext, SCAN_CTX, ScanContext, ScannerSlot};

/// One planned invocation: a function call from a scanner. Immutable
/// once the planner finishes.
pub(crate) struct Task {
    pub scanner_idx: usize,
    pub task_name: String,
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

/// Channel message — emitted by worker tasks (Started, Completed). The
/// scheduler driver consumes these to drive the DAG. Print/Println from
/// Rune `print`/`println` flow over a separate
/// [`gage_runtime::RuntimeOutput`] channel.
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

/// Build a single DAG over every (scanner, task) declaration.
#[allow(clippy::indexing_slicing)]
pub(crate) fn plan(
    scanners: &[ScannerSlot],
    scanner_tasks: &[HashMap<String, TaskDef>],
    _run: &Arc<RunContext>,
) -> Result<Plan, PlanError> {
    let mut graph = Graph::<usize, ()>::new();
    let mut tasks: Vec<Task> = Vec::new();
    let mut warnings: Vec<PlanWarning> = Vec::new();
    // (scanner_idx, task_name) -> node index
    let mut node_index: HashMap<(usize, String), NodeIndex> = HashMap::new();

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

    for &(scanner_idx, task_name, _) in &planned {
        let task_idx = tasks.len();
        tasks.push(Task {
            scanner_idx,
            task_name: task_name.clone(),
        });
        let node = graph.add_node(task_idx);
        node_index.insert((scanner_idx, task_name.clone()), node);
    }

    // Build plan-wide `item_name -> [(scanner_idx, task_name)]` indexes
    // from `writes`, one per item kind. Dependency resolution spans all
    // scanners — a consumer in scanner A can depend on a producer in
    // scanner B.
    let mut note_writes: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    let mut issue_writes: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (scanner_idx, defs) in scanner_tasks.iter().enumerate() {
        for (task_name, def) in defs {
            for name in def.notes.writes.keys() {
                note_writes
                    .entry(name.clone())
                    .or_default()
                    .push((scanner_idx, task_name.clone()));
            }
            for name in def.issues.writes.keys() {
                issue_writes
                    .entry(name.clone())
                    .or_default()
                    .push((scanner_idx, task_name.clone()));
            }
        }
    }

    // Wire dependencies.
    //
    // Each `wants` entry is a `*`-glob pattern over item names. For every
    // pattern, find all tasks in the plan whose `writes` includes a
    // matching name, and add an edge from each producer to the consumer.
    //
    // An unsatisfied `wants` (no task in the plan writes a matching item)
    // is recorded as a warning, not an error — the consumer still runs.
    for &(scanner_idx, task_name, def) in &planned {
        for (wants, writes_index, kind) in [
            (&def.notes.wants, &note_writes, "note"),
            (&def.issues.wants, &issue_writes, "issue"),
        ] {
            for want in wants {
                let mut matched = false;
                for (written_name, producers) in writes_index {
                    if !glob_match(want, written_name) {
                        continue;
                    }
                    matched = true;
                    for (producer_scanner, producer_task) in producers {
                        if *producer_scanner == scanner_idx && producer_task == task_name {
                            // A task wanting an item it writes itself is a
                            // no-op dependency — would create a self-loop.
                            // Skip.
                            continue;
                        }
                        let from = *node_index
                            .get(&(*producer_scanner, producer_task.clone()))
                            .unwrap();
                        let to = *node_index.get(&(scanner_idx, task_name.clone())).unwrap();
                        graph.update_edge(from, to, ());
                    }
                }
                if !matched {
                    warnings.push(PlanWarning {
                        scanner: scanners[scanner_idx].name.clone(),
                        task: task_name.clone(),
                        message: format!("wants {kind} '{want}' but no task writes it"),
                    });
                }
            }
        }
    }

    // Cycle detection via SCC over the full graph
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

    // Build adjacency arrays indexed by task index
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
    }

    Ok(Plan {
        tasks,
        downstream,
        deps,
        warnings,
    })
}

/// Run a single plan, dispatching tasks to a pool of `jobs` workers.
pub(crate) async fn run_plan(
    plan: Plan,
    scanners: Arc<Vec<ScannerSlot>>,
    run: Arc<RunContext>,
    jobs: usize,
    cancel: CancellationToken,
    mut on_event: impl FnMut(ScanEvent) + Send,
) -> Result<(RunSummary, Vec<(String, TaskOutcome)>), RunError> {
    let jobs = jobs.max(1);
    let plan_total = plan.tasks.len();

    // Surface planner warnings before any task runs
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
    let outcomes = run_tasks(
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

    Ok((
        RunSummary {
            scan_id: status.scan_id,
            total: plan_total,
            completed: accounting.completed,
            failed: accounting.failed,
            skipped: accounting.skipped,
        },
        outcomes,
    ))
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
) -> Result<Vec<(String, TaskOutcome)>, RunError> {
    let task_count = plan.tasks.len();
    let tasks = Arc::new(plan.tasks);
    let downstream = Arc::new(plan.downstream);
    let deps_remaining: Arc<Vec<AtomicU32>> =
        Arc::new(plan.deps.iter().map(|d| AtomicU32::new(*d)).collect());

    let (ready_tx, ready_rx) = mpsc::unbounded_channel::<usize>();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<WorkerMsg>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<gage_runtime::RuntimeOutput>();

    // Seed initial ready set
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
        let out_tx = out_tx.clone();
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
                let outcome = dispatch_task(task, &scanners, &run, &out_tx).await;
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
    drop(out_tx);

    // Per-task dispatch times and terminal outcomes, keyed for the
    // end-of-run metadata write. Tasks never dispatched (cancellation)
    // have no outcome.
    let mut started_at: Vec<Option<(i64, Instant)>> = vec![None; task_count];
    let mut outcomes: Vec<(String, TaskOutcome)> = Vec::with_capacity(task_count);

    let mut completed = 0usize;
    let mut canceled = false;
    while completed < task_count {
        enum Tick {
            Msg(Option<WorkerMsg>),
            Out(Option<gage_runtime::RuntimeOutput>),
        }
        let tick = tokio::select! {
            biased;
            _ = cancel.cancelled(), if !canceled => {
                canceled = true;
                for h in &worker_handles {
                    h.abort();
                }
                continue;
            }
            msg = msg_rx.recv() => Tick::Msg(msg),
            out = out_rx.recv() => Tick::Out(out),
        };
        let msg = match tick {
            Tick::Out(Some(out)) => {
                match out {
                    gage_runtime::RuntimeOutput::Print(s) => on_event(ScanEvent::Print { s }),
                    gage_runtime::RuntimeOutput::Println(s) => on_event(ScanEvent::Println { s }),
                }
                continue;
            }
            Tick::Out(None) => continue,
            Tick::Msg(msg) => msg,
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
                });
                started_at[task_idx] = Some((gage_core::datetime::now_ms(), Instant::now()));
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
                            message: msg.clone(),
                        });
                    }
                    TaskResult::SkippedByFault => {
                        accounting.skipped += 1;
                        // Fault-skips remove work from the pipeline
                        // rather than counting toward progress
                        status.total = status.total.saturating_sub(1);
                    }
                }
                let (task_status, error) = match &outcome {
                    TaskResult::Ok => (TaskStatus::Completed, None),
                    TaskResult::Error(msg) | TaskResult::VmError(msg) => {
                        (TaskStatus::Failed, Some(msg.clone()))
                    }
                    TaskResult::SkippedByFault => (TaskStatus::Skipped, None),
                };
                // Skipped tasks never ran; their dispatch instant is
                // not a start time.
                let timing = match task_status {
                    TaskStatus::Skipped => None,
                    _ => started_at[task_idx],
                };
                outcomes.push((
                    scanners[task.scanner_idx].name.clone(),
                    TaskOutcome {
                        name: task.task_name.clone(),
                        status: task_status,
                        started: timing.map(|(ms, _)| ms),
                        elapsed_ms: timing.map(|(_, t)| t.elapsed().as_millis() as u64),
                        error,
                    },
                ));
                on_event(ScanEvent::Status(status.clone()));
                completed += 1;
                for &down in &downstream[task_idx] {
                    let prev = deps_remaining[down].fetch_sub(1, Ordering::SeqCst);
                    if prev == 1 {
                        ready_tx.send(down).expect("ready channel open");
                    }
                }
            }
        }
    }
    // Drain any remaining Print/Println so output isn't lost.
    while let Ok(out) = out_rx.try_recv() {
        match out {
            gage_runtime::RuntimeOutput::Print(s) => on_event(ScanEvent::Print { s }),
            gage_runtime::RuntimeOutput::Println(s) => on_event(ScanEvent::Println { s }),
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
    Ok(outcomes)
}

#[allow(clippy::indexing_slicing)]
async fn dispatch_task(
    task: &Task,
    scanners: &[ScannerSlot],
    run: &Arc<RunContext>,
    out_tx: &mpsc::UnboundedSender<gage_runtime::RuntimeOutput>,
) -> TaskResult {
    let slot = &scanners[task.scanner_idx];

    if slot.fault.lock().unwrap().is_some() {
        return TaskResult::SkippedByFault;
    }

    let task_name = task.task_name.clone();
    let rt = slot.rt.clone();
    let unit = slot.unit.clone();
    let sources = slot.sources.clone();
    let ctx = Arc::new(ScanContext {
        scanner_name: slot.name.clone(),
        params: slot.params.clone(),
        run: run.clone(),
        db: slot.db.clone(),
        runtime_tx: out_tx.clone(),
        rt: rt.clone(),
        unit: unit.clone(),
        sources: sources.clone(),
    });
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
                    Ok(Err(err)) if gage_runtime::ignore::is_ignore(&err) => TaskResult::Ok,
                    Ok(Err(err)) => TaskResult::Error(crate::error::render_task_error(err)),
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
