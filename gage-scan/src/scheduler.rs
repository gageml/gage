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
//! - `required_by` patterns contribute ordering edges the same way
//!   `wants` does, without the unmatched warning. Pull-in (scheduling a
//!   task whose scanner was not selected) happens before compilation,
//!   in `ScannerRegistry::required_tasks`; by plan time a pulled-in
//!   task is an ordinary task.
//! - Cycle detection runs at plan time over the full graph.
//! - Worker pool: N tokio tasks pulling from an unbounded ready queue.
//!   Per-scanner concurrency is unrestricted — each task builds a
//!   fresh [`rune::Vm`] from the scanner's shared compilation
//!   artifacts.
//!
//! Note: scanner module-level state is NOT preserved across tasks.
//! Tasks are independent invocations and communicate via notes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use petgraph::Graph;
use petgraph::algo::tarjan_scc;
use petgraph::graph::NodeIndex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::event::{ActiveTask, RunStatus, RunSummary, ScanEvent, WorkerStatus};
use gage_core::glob::glob_match;
use gage_core::task::task_display;
use gage_db::scan::{ScanError, TaskFinish, TaskMetadata, TaskStatus, finish_task, start_task};
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
/// `scanner_names` and `scanner_tasks` are parallel: entry `i` of each
/// describes the same scanner.
#[allow(clippy::indexing_slicing)]
pub(crate) fn plan(
    scanner_names: &[String],
    scanner_tasks: &[HashMap<String, TaskDef>],
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
        scanner_names[a.0]
            .cmp(&scanner_names[b.0])
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
    // `required_by` patterns contribute ordering edges exactly as
    // `wants` does, without the unmatched warning: an unmatched
    // `required_by` just means no writer pulled the task in.
    for &(scanner_idx, task_name, def) in &planned {
        for (wants, writes_index, kind, warn_unmatched) in [
            (&def.notes.wants, &note_writes, "note", true),
            (&def.issues.wants, &issue_writes, "issue", true),
            (&def.notes.required_by, &note_writes, "note", false),
            (&def.issues.required_by, &issue_writes, "issue", false),
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
                if !matched && warn_unmatched {
                    warnings.push(PlanWarning {
                        scanner: scanner_names[scanner_idx].clone(),
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
                    task_display(&scanner_names[t.scanner_idx], &t.task_name)
                })
                .collect();
            return Err(PlanError {
                scanner: scanner_names[tasks[*graph.node_weight(scc[0]).unwrap()].scanner_idx]
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
) -> Result<RunSummary, RunError> {
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
    let canceled = run_tasks(
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
        canceled,
    })
}

struct RunAccounting {
    completed: usize,
    failed: usize,
    skipped: usize,
}

pub enum RunError {
    Channel,
    Db(ScanError),
}

/// Dispatch the plan's tasks. Returns whether the run was canceled;
/// on cancellation, in-flight and pending task rows are finalized as
/// `canceled` before returning, so the db is terminal either way.
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
) -> Result<bool, RunError> {
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
                    gage_runtime::RuntimeOutput::Progress {
                        scanner,
                        task,
                        pos,
                        total,
                    } => {
                        // Fold into the worker slot running this task
                        // — (scanner, task) is unique in the plan — and
                        // re-emit the full snapshot. A report from a
                        // task no longer on a worker (completed while
                        // the message was in flight) is stale; drop it.
                        let slot = status.workers.iter_mut().find_map(|w| {
                            w.current
                                .as_mut()
                                .filter(|c| c.scanner == scanner && c.task == task)
                        });
                        if let Some(current) = slot {
                            current.progress = Some((pos, total));
                            on_event(ScanEvent::Status(status.clone()));
                        }
                    }
                    gage_runtime::RuntimeOutput::AgentPool {
                        scanner,
                        task,
                        delta,
                    } => {
                        // Fold pool occupancy into the worker slot, as
                        // for Progress; a report from a task no longer
                        // on a worker is stale and dropped. Worked time
                        // is a stopwatch: it runs except over spells
                        // where every one of the task's agents is
                        // waiting on the pool.
                        let slot = status.workers.iter_mut().find_map(|w| {
                            w.current
                                .as_mut()
                                .filter(|c| c.scanner == scanner && c.task == task)
                        });
                        if let Some(current) = slot {
                            use gage_runtime::AgentPoolDelta as Delta;
                            match delta {
                                Delta::Queued => current.agents_waiting += 1,
                                Delta::Acquired => {
                                    current.agents_waiting =
                                        current.agents_waiting.saturating_sub(1);
                                    current.agents_active += 1;
                                }
                                Delta::Released => {
                                    current.agents_active = current.agents_active.saturating_sub(1);
                                }
                            }
                            match (current.pool_blocked(), current.working_since) {
                                (true, Some(since)) => {
                                    current.worked += since.elapsed();
                                    current.working_since = None;
                                }
                                (false, None) => {
                                    current.working_since = Some(std::time::Instant::now());
                                }
                                _ => {}
                            }
                            on_event(ScanEvent::Status(status.clone()));
                        }
                    }
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
                status.workers[worker_id].current = Some(ActiveTask {
                    scanner: slot.name.clone(),
                    task: task.task_name.clone(),
                    progress: None,
                    agents_waiting: 0,
                    agents_active: 0,
                    worked: std::time::Duration::ZERO,
                    working_since: Some(std::time::Instant::now()),
                });
                let now_ms = gage_core::datetime::now_ms();
                {
                    let conn = slot.db.lock().unwrap();
                    start_task(&conn, &run.scan_id, &slot.name, &task.task_name, now_ms)
                        .map_err(RunError::Db)?;
                }
                on_event(ScanEvent::Status(status.clone()));
            }
            WorkerMsg::Completed {
                worker_id,
                task_idx,
                outcome,
            } => {
                let task = &tasks[task_idx];
                let active = status.workers[worker_id].current.take();
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
                        // A skip resolves its task; progress counts it
                        // so the bar's total stays the planned task
                        // count shown in the task list
                        status.progress += 1;
                    }
                }
                let (task_status, error) = match &outcome {
                    TaskResult::Ok => (TaskStatus::Completed, None),
                    TaskResult::Error(msg) | TaskResult::VmError(msg) => {
                        (TaskStatus::Failed, Some(msg.as_str()))
                    }
                    TaskResult::SkippedByFault => (TaskStatus::Skipped, None),
                };
                // Skipped tasks never ran; `finish_task` clears their
                // dispatch timestamp.
                let stopped_ms = match task_status {
                    TaskStatus::Skipped => None,
                    _ => Some(gage_core::datetime::now_ms()),
                };
                let metadata = match task_status {
                    TaskStatus::Skipped => None,
                    _ => active.map(|a| TaskMetadata {
                        worked_ms: a.worked_total().as_millis() as u64,
                    }),
                };
                {
                    let slot = &scanners[task.scanner_idx];
                    let conn = slot.db.lock().unwrap();
                    finish_task(
                        &conn,
                        &run.scan_id,
                        &slot.name,
                        &task.task_name,
                        TaskFinish {
                            status: task_status,
                            stopped_ms,
                            error,
                            metadata: metadata.as_ref(),
                        },
                    )
                    .map_err(RunError::Db)?;
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
        }
    }
    // Drain any remaining Print/Println so output isn't lost.
    while let Ok(out) = out_rx.try_recv() {
        match out {
            gage_runtime::RuntimeOutput::Print(s) => on_event(ScanEvent::Print { s }),
            gage_runtime::RuntimeOutput::Println(s) => on_event(ScanEvent::Println { s }),
            // Every task is terminal at this point; progress is stale
            gage_runtime::RuntimeOutput::Progress { .. } => {}
            gage_runtime::RuntimeOutput::AgentPool { .. } => {}
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
        // Workers are joined, so no task is still writing; finalize
        // the in-flight rows the aborted workers left behind and the
        // pending rows that will never dispatch.
        if let Some(slot) = scanners.first() {
            let conn = slot.db.lock().unwrap();
            gage_db::scan::cancel_unfinished_tasks(
                &conn,
                &run.scan_id,
                gage_core::datetime::now_ms(),
            )
            .map_err(RunError::Db)?;
        }
    }
    Ok(canceled)
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
        task_name: task_name.clone(),
        params: slot.params.clone(),
        run: run.clone(),
        db: slot.db.clone(),
        runtime_tx: out_tx.clone(),
        rt: rt.clone(),
        unit: unit.clone(),
        sources: sources.clone(),
        task_fault: Mutex::new(None),
    });
    let label_task = task_name.clone();
    let ctx_probe = ctx.clone();
    let outcome = SCAN_CTX
        .scope(ctx, async move {
            let vm = rune::Vm::new(rt, unit);
            let execution = match vm.send_execute([task_name.as_str()], ()) {
                Ok(e) => e,
                Err(e) => return TaskResult::VmError(render_vm_err(&e, &sources)),
            };
            match execution.complete().await {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "takes the VM execution's return value; the scheduler \
                              holds the only live handle"
                )]
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

    // A fault the runtime detected (claude not logged in) aborts the
    // task's VM only as an unwind vehicle; the recorded message is the
    // task's real failure. Report it as a plain task error and discard
    // the VM error's panic-shaped rendering.
    let outcome = match ctx_probe.task_fault.lock().unwrap().take() {
        Some(msg) => TaskResult::Error(msg),
        None => outcome,
    };

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

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use std::collections::BTreeMap;

    use gage_registry::scanner::TaskDepsDef;

    use super::*;

    /// `(task_name, notes_wants, notes_writes, issues_wants, issues_writes)`
    type TaskEntry<'a> = (
        &'a str,
        &'a [&'a str],
        &'a [&'a str],
        &'a [&'a str],
        &'a [&'a str],
    );

    /// Build one scanner's task map from [`TaskEntry`] tuples.
    fn tasks(entries: &[TaskEntry<'_>]) -> HashMap<String, TaskDef> {
        entries
            .iter()
            .map(|(name, nw, nwr, iw, iwr)| {
                (
                    (*name).to_string(),
                    TaskDef {
                        name: (*name).to_string(),
                        notes: deps(nw, nwr),
                        issues: deps(iw, iwr),
                    },
                )
            })
            .collect()
    }

    fn deps(wants: &[&str], writes: &[&str]) -> TaskDepsDef {
        TaskDepsDef {
            wants: wants.iter().map(|s| (*s).to_string()).collect(),
            writes: writes
                .iter()
                .map(|s| ((*s).to_string(), String::new()))
                .collect::<BTreeMap<_, _>>(),
            required_by: Vec::new(),
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// Index of `scanner::task` in the plan.
    fn idx(plan: &Plan, scanner_names: &[String], scanner: &str, task: &str) -> usize {
        plan.tasks
            .iter()
            .position(|t| scanner_names[t.scanner_idx] == scanner && t.task_name == task)
            .unwrap_or_else(|| panic!("no task {scanner}::{task} in plan"))
    }

    /// Assert `producer` is upstream of `consumer`: the edge is in
    /// `downstream` and the consumer's in-degree counts it.
    fn assert_edge(plan: &Plan, producer: usize, consumer: usize) {
        assert!(
            plan.downstream[producer].contains(&consumer),
            "expected edge {producer} -> {consumer}, downstream: {:?}",
            plan.downstream,
        );
        assert!(plan.deps[consumer] > 0, "consumer {consumer} has no deps");
    }

    #[test]
    fn note_wants_orders_consumer_after_producer() {
        let scanners = names(&["a", "b"]);
        let plan = plan(
            &scanners,
            &[
                tasks(&[("write", &[], &["finding"], &[], &[])]),
                tasks(&[("read", &["finding"], &[], &[], &[])]),
            ],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let producer = idx(&plan, &scanners, "a", "write");
        let consumer = idx(&plan, &scanners, "b", "read");
        assert_edge(&plan, producer, consumer);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn issue_wants_orders_consumer_after_producer() {
        let scanners = names(&["a", "b"]);
        let plan = plan(
            &scanners,
            &[
                tasks(&[("write", &[], &[], &[], &["bug"])]),
                tasks(&[("read", &[], &[], &["bug"], &[])]),
            ],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let producer = idx(&plan, &scanners, "a", "write");
        let consumer = idx(&plan, &scanners, "b", "read");
        assert_edge(&plan, producer, consumer);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn note_and_issue_names_do_not_cross_match() {
        let scanners = names(&["a", "b"]);
        let plan = plan(
            &scanners,
            &[
                // writes a *note* named "bug"
                tasks(&[("write", &[], &["bug"], &[], &[])]),
                // wants an *issue* named "bug"
                tasks(&[("read", &[], &[], &["bug"], &[])]),
            ],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let consumer = idx(&plan, &scanners, "b", "read");
        assert_eq!(plan.deps[consumer], 0);
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].message.contains("wants issue 'bug'"));
    }

    #[test]
    fn star_wants_depends_on_every_issue_writer() {
        // The `issue-summary` shape: one consumer downstream of every
        // issue writer in the plan.
        let scanners = names(&["a", "b", "summary"]);
        let plan = plan(
            &scanners,
            &[
                tasks(&[("write", &[], &[], &[], &["one"])]),
                tasks(&[("write", &[], &[], &[], &["two"])]),
                tasks(&[("main", &[], &["issue-summary"], &["*"], &[])]),
            ],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let consumer = idx(&plan, &scanners, "summary", "main");
        assert_edge(&plan, idx(&plan, &scanners, "a", "write"), consumer);
        assert_edge(&plan, idx(&plan, &scanners, "b", "write"), consumer);
        assert_eq!(plan.deps[consumer], 2);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn prefix_glob_matches_writer() {
        let scanners = names(&["a", "b"]);
        let plan = plan(
            &scanners,
            &[
                tasks(&[("write", &[], &["session.finding.abc"], &[], &[])]),
                tasks(&[("read", &["session.finding.*"], &[], &[], &[])]),
            ],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        assert_edge(
            &plan,
            idx(&plan, &scanners, "a", "write"),
            idx(&plan, &scanners, "b", "read"),
        );
    }

    #[test]
    fn unsatisfied_wants_warns_and_still_plans_the_task() {
        let scanners = names(&["a"]);
        let plan = plan(
            &scanners,
            &[tasks(&[("read", &["nobody-writes-this"], &[], &[], &[])])],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(
            plan.deps[0], 0,
            "an unsatisfied want must not block dispatch"
        );
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.warnings[0].scanner, "a");
        assert_eq!(plan.warnings[0].task, "read");
        assert!(
            plan.warnings[0]
                .message
                .contains("wants note 'nobody-writes-this'")
        );
    }

    #[test]
    fn task_wanting_what_it_writes_has_no_self_edge() {
        let scanners = names(&["a"]);
        let plan = plan(
            &scanners,
            &[tasks(&[("both", &["finding"], &["finding"], &[], &[])])],
        )
        .unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(plan.deps[0], 0);
        assert!(plan.downstream[0].is_empty());
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn cycle_is_a_plan_error() {
        let scanners = names(&["a", "b"]);
        let err = plan(
            &scanners,
            &[
                tasks(&[("one", &["y"], &["x"], &[], &[])]),
                tasks(&[("two", &["x"], &["y"], &[], &[])]),
            ],
        )
        .err()
        .expect("mutual wants should cycle");
        assert!(err.message.contains("cycle in task dependencies"));
    }
}
