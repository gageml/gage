//! Second-generation scan facility for the schema rethink, paired
//! with `gage-runtime2`. The first-generation crates (`gage-runtime`,
//! `gage-scan`, `gage-query`, `gage-db`) stay untouched and working,
//! with their tests, while this generation is built brick by brick on
//! `gage-store`, `gage-session`, and `gage-query2`. When this
//! generation is complete it is promoted onto the original crate
//! names.
//!
//! Rules:
//!
//! - Do not modify the first-generation crates, except to make an
//!   existing item public so it can be called from here.
//! - Do not build on `gage-db`, or on `gage-query`'s store-bound
//!   tables.
//! - Reuse as needed by calling into `gage-runtime`, `gage-scan`, and
//!   `gage-query`. Do not copy code from them; a copy loses its
//!   revision history at promotion.
//!
//! This crate owns task orchestration: it compiles scanners against
//! the `gage-runtime2` context, plans their tasks as one DAG (see
//! [`plan`]), runs the tasks through a worker pool as their upstream
//! tasks finish, and records the run in [`staging`] and then the
//! store. The runtime is a pure event emitter: [`scan`] hands each
//! [`Event`] to the caller's sink, which owns rendering.
//!
//! Rule: code in this crate and in `gage-runtime2` never calls
//! `println!` or `eprintln!`. Every line meant for a person is emitted
//! as [`Event::Scan`] output, which the scan writes to its `logs/out`
//! or `logs/err` before the sink shows it, so the stored record and
//! the terminal hold the same text. Runtime diagnostics go through
//! `tracing` and reach the record through [`trace`].

pub mod plan;
pub mod staging;
pub mod trace;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use gage_core::datetime::now_ms;
use gage_core::uuid::{new_uuid, short_uuid};
use gage_registry::scanner::{Scanner, TaskDef};
use gage_runtime2::source::{SourceError, SourceFile, source_files};
use gage_runtime2::{
    CURRENT_RUNTIME_SCHEME, Level, OUTPUT_SINK, Output, OutputSink, SCAN_CTX, ScanContext,
    ScanDatasetRef, TaskOutput,
};
use gage_scan::error::render_task_error;
use gage_store::{
    DatasetStore, IssueStore, NoteStore, ScanAttrs, ScanStore, Store, StoreError, TaskAttrs,
    TaskCounts, TaskStatus,
};
use rune::runtime::{RuntimeContext, Unit, Value, VmError};
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources, Vm};
use serde_json as json;
use tokio::sync::mpsc;
use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::plan::{Plan, PlanError, PlannedScanner, Selection};
use crate::staging::{Logs, ScannerPlan, Staging, State};
use crate::trace::{LOG_SCOPE, LogScope};

/// One item of run output, in the order it happened.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Task output, already recorded in the scan's `logs/`
    Output(TaskOutput),
    /// The scan's own output for a person, already recorded in the
    /// scan's `logs/`
    Scan(ScanOutput),
    /// A plan warning, given before any task runs and already
    /// recorded in the scan's `logs/records`: the task wants a note
    /// no planned task writes
    Warning {
        scanner: String,
        task: String,
        message: String,
    },
    TaskStarted {
        scanner: String,
        task: String,
    },
    /// A task reached a terminal status. `error` is the rendered
    /// failure for `Failed`.
    TaskFinished {
        scanner: String,
        task: String,
        status: TaskStatus,
        error: Option<String>,
    },
}

/// A line of the scan's own output. The text is written verbatim,
/// newline included, to `logs/out` or `logs/err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutput {
    Out(String),
    Err(String),
}

#[derive(Debug)]
pub enum Error {
    Compile {
        name: String,
        diagnostics: String,
    },
    MissingTask {
        scanner: String,
        task: String,
    },
    /// The scanner's source files cannot be enumerated or stored
    Source {
        name: String,
        source: SourceError,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Compile { name, diagnostics } => {
                write!(f, "scanner {name} failed to compile\n{diagnostics}")
            }
            Error::MissingTask { scanner, task } => {
                write!(
                    f,
                    "scanner {scanner} declares task {task} but defines no such function"
                )
            }
            Error::Source { name, source } => write!(f, "scanner {name}: {source}"),
        }
    }
}

impl std::error::Error for Error {}

/// A scanner compiled and ready to run. The `Vm` is not stored; a
/// fresh one is built per task.
pub struct CompiledScanner {
    name: String,
    /// The planned tasks by name: every declared task for a selected
    /// scanner, the pulled tasks for one pulled in by `required_by`
    tasks: BTreeMap<String, TaskDef>,
    selection: Selection,
    /// The scanner's resolved params, read by `params()` in its tasks
    params: Option<json::Value>,
    /// The files the scanner is built from, recorded with the scan
    source_files: Vec<SourceFile>,
    rt: RuneArc<RuntimeContext>,
    unit: RuneArc<Unit>,
    sources: Arc<Sources>,
}

impl CompiledScanner {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The compiled artifacts a worker needs to run one of the
    /// scanner's tasks.
    fn task_unit(&self) -> TaskUnit {
        TaskUnit {
            rt: self.rt.clone(),
            unit: self.unit.clone(),
            sources: Arc::clone(&self.sources),
            params: self.params.clone(),
        }
    }
}

/// Compile a scanner and verify that every declared task maps to a
/// function of the same name. A scanner that fails here is a full
/// stop for the caller: nothing has run yet. A scanner selected by
/// name or file plans every declared task; one pulled in by
/// `required_by` carries the pulled tasks in `only_tasks` and plans
/// those alone. Every declared task is verified regardless.
pub fn compile(scanner: &Scanner<'_>) -> Result<CompiledScanner, Error> {
    let def = scanner.def;
    let (selection, only) = match &scanner.only_tasks {
        Some(tasks) => (Selection::RequiredBy, Some(tasks.as_slice())),
        None => (Selection::Explicit, None),
    };
    let context = gage_runtime2::context().unwrap();
    let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();

    let mut sources = Sources::new();
    sources
        .insert(Source::with_path(&def.name, def.source(), &def.path).unwrap())
        .unwrap();

    let mut diagnostics = Diagnostics::new();
    let result = rune::prepare(&mut sources)
        .with_context(&context)
        .with_diagnostics(&mut diagnostics)
        .build();

    // Diagnostics render to a plain-text buffer rather than stderr so
    // the caller controls presentation.
    let rendered = if diagnostics.is_empty() {
        String::new()
    } else {
        let mut buf = rune::termcolor::Buffer::no_color();
        diagnostics.emit(&mut buf, &sources).unwrap();
        String::from_utf8(buf.into_inner()).unwrap()
    };

    let unit = match result {
        Ok(unit) => {
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            RuneArc::try_new(unit).unwrap()
        }
        Err(_) => {
            return Err(Error::Compile {
                name: def.name.clone(),
                diagnostics: rendered,
            });
        }
    };

    // Enumerated after the build so a malformed scanner reports
    // Rune's diagnostics rather than a tokenizer error.
    let files = source_files(&def.path).map_err(|source| Error::Source {
        name: def.name.clone(),
        source,
    })?;

    let vm = Vm::new(rt.clone(), unit.clone());
    for task in def.tasks.keys() {
        if vm.lookup_function([task.as_str()]).is_err() {
            return Err(Error::MissingTask {
                scanner: def.name.clone(),
                task: task.clone(),
            });
        }
    }

    let tasks = def
        .tasks
        .iter()
        .filter(|(name, _)| only.is_none_or(|only| only.contains(name)))
        .map(|(name, task)| (name.clone(), task.clone()))
        .collect();

    Ok(CompiledScanner {
        name: def.name.clone(),
        tasks,
        selection,
        params: scanner.params.clone(),
        source_files: files,
        rt,
        unit,
        sources: Arc::new(sources),
    })
}

/// A failure of the scan lifecycle itself, as opposed to a task
/// failure, which the scan records and continues past.
#[derive(Debug)]
pub enum ScanError {
    /// Two scanners share a name; `tasks/<scanner>/` cannot hold both
    DuplicateScanner(String),
    /// The tasks cannot be planned
    Plan(PlanError),
    /// Staging could not be written
    Staging(io::Error),
    /// The scan could not be applied to the store
    Store(StoreError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::DuplicateScanner(name) => {
                write!(f, "scanner {name} is given more than once")
            }
            ScanError::Plan(e) => write!(f, "planning the scan: {e}"),
            ScanError::Staging(e) => write!(f, "writing scan staging: {e}"),
            ScanError::Store(e) => write!(f, "writing scan to the store: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<io::Error> for ScanError {
    fn from(e: io::Error) -> Self {
        ScanError::Staging(e)
    }
}

impl From<StoreError> for ScanError {
    fn from(e: StoreError) -> Self {
        ScanError::Store(e)
    }
}

/// Where a scan stages, how many tasks it runs at once, and what it
/// records about its runtime.
pub struct ScanConfig<'a> {
    /// The staging root, `staging/` under Gage home in production
    pub staging_root: &'a std::path::Path,
    /// The Gage build version; the scan records
    /// `<CURRENT_RUNTIME_SCHEME> <version>` as its `runtime`
    pub gage_version: &'a str,
    /// The commit SHA of the dataset to scan, linked from the scan as
    /// `dataset.link`. `None` runs the scanners with no dataset.
    pub dataset: Option<&'a str>,
    /// Tasks run at once. Treated as at least 1.
    pub jobs: usize,
}

/// What a finished scan wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub id: String,
    pub commit_sha: String,
    pub attrs: ScanAttrs,
}

/// Plan every task of every scanner, run them through a pool of
/// `config.jobs` workers as their upstream tasks finish, and record
/// the scan in `store`.
///
/// The scan is staged under `config.staging_root/<id>/` while it runs
/// (see [`staging`]) and applied to the store at its terminal state,
/// after which the staging directory is removed. Output and task
/// status reach `on_event` as they happen. A failed task is recorded
/// and the run continues; the tasks ordered after it run and read
/// what exists.
/// Cancelling `cancel` abandons the running tasks at their next await
/// point, marks them and every task not yet started `canceled`, and
/// applies what ran.
pub async fn scan(
    store: &Store,
    config: &ScanConfig<'_>,
    scanners: &[CompiledScanner],
    cancel: &CancellationToken,
    on_event: impl FnMut(Event),
) -> Result<ScanOutcome, ScanError> {
    check_unique_names(scanners)?;
    let planned: Vec<PlannedScanner<'_>> = scanners
        .iter()
        .map(|s| PlannedScanner {
            name: &s.name,
            tasks: &s.tasks,
            selection: s.selection.clone(),
        })
        .collect();
    let plan = plan::plan(&planned).map_err(ScanError::Plan)?;
    let id = new_uuid();
    // Tasks read the dataset through their own handle: the store's git
    // reader is single-threaded, and this one stays free for the apply
    let dataset = match config.dataset {
        Some(sha) => Some(ScanDatasetRef {
            id: DatasetStore::from(store).at_commit(sha)?.id,
            commit_sha: sha.to_string(),
        }),
        None => None,
    };
    let task_names: Vec<Vec<String>> = scanners
        .iter()
        .map(|s| s.tasks.keys().cloned().collect())
        .collect();
    let scanner_plans: Vec<ScannerPlan<'_>> = scanners
        .iter()
        .zip(&task_names)
        .map(|(s, tasks)| ScannerPlan {
            name: &s.name,
            tasks,
            sources: &s.source_files,
        })
        .collect();
    let staging = Staging::create(config.staging_root, &id, config.dataset, &scanner_plans)?;
    staging.write_plan(&plan.to_json())?;
    let scan_ctx = ScanContext::new(id.clone(), dataset, store.path(), staging.runtime_paths())?;
    trace::install_panic_hook();
    let scope = LogScope {
        scan_dir: staging.scan_dir(),
        task: None,
        failure: Arc::new(Mutex::new(None)),
    };
    // One channel for the whole scan: every task sends through it and
    // the run loop is the only receiver, so receive order is the order
    // of the scan
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let run = Run {
        id,
        config,
        units: scanners
            .iter()
            .map(|s| (s.name.clone(), s.task_unit()))
            .collect(),
        plan: Arc::new(plan),
        staging,
        cancel,
        scope: scope.clone(),
        scan_ctx,
        output_tx,
        output_rx,
        on_event,
    };
    LOG_SCOPE.scope(scope, run.execute(store)).await
}

fn check_unique_names(scanners: &[CompiledScanner]) -> Result<(), ScanError> {
    for (i, scanner) in scanners.iter().enumerate() {
        if scanners.iter().take(i).any(|s| s.name == scanner.name) {
            return Err(ScanError::DuplicateScanner(scanner.name.clone()));
        }
    }
    Ok(())
}

/// One scan in progress.
struct Run<'a, F: FnMut(Event)> {
    id: String,
    config: &'a ScanConfig<'a>,
    /// Compiled artifacts by scanner name
    units: HashMap<String, TaskUnit>,
    plan: Arc<Plan>,
    staging: Staging,
    cancel: &'a CancellationToken,
    scope: LogScope,
    scan_ctx: ScanContext,
    output_tx: mpsc::UnboundedSender<TaskOutput>,
    output_rx: mpsc::UnboundedReceiver<TaskOutput>,
    on_event: F,
}

/// The live state of dispatch: what each task has reached and what
/// is ready to start.
struct Dispatch {
    /// Remaining in-degree per task
    deps: Vec<u32>,
    /// Terminal status per task, `None` until it has one
    status: Vec<Option<TaskStatus>>,
    /// Start time per task, `None` until it starts
    started: Vec<Option<i64>>,
    /// Tasks with no remaining upstream task, in release order
    ready: VecDeque<usize>,
    /// Running tasks by tokio task id
    running: HashMap<Id, usize>,
    counts: TaskCounts,
}

impl<F: FnMut(Event)> Run<'_, F> {
    #[expect(
        clippy::indexing_slicing,
        reason = "task indices are plan-internal and bounded by construction"
    )]
    async fn execute(mut self, store: &Store) -> Result<ScanOutcome, ScanError> {
        let plan = Arc::clone(&self.plan);
        tracing::info!("scan {} started with {} tasks", self.id, plan.tasks.len());
        let mut scan_logs = self.staging.scan_logs();
        let started = now_ms();
        for t in &plan.tasks {
            for pattern in &t.unmatched {
                let message = format!("wants note '{pattern}' but no task writes it");
                scan_logs.record(Level::Warn, &t.label(), &message)?;
                (self.on_event)(Event::Warning {
                    scanner: t.scanner.clone(),
                    task: t.task.clone(),
                    message,
                });
            }
        }
        let mut dispatch = Dispatch {
            deps: plan.deps.clone(),
            status: vec![None; plan.tasks.len()],
            started: vec![None; plan.tasks.len()],
            ready: (0..plan.tasks.len())
                .filter(|i| plan.deps[*i] == 0)
                .collect(),
            running: HashMap::new(),
            counts: TaskCounts {
                total: plan.tasks.len(),
                ..TaskCounts::default()
            },
        };
        let canceled = self.run_tasks(&plan, &mut dispatch, &mut scan_logs).await?;

        let attrs = ScanAttrs {
            runtime: format!("{CURRENT_RUNTIME_SCHEME} {}", self.config.gage_version),
            started,
            stopped: now_ms(),
            canceled,
            tasks: dispatch.counts,
        };
        self.say(
            &mut scan_logs,
            ScanOutput::Out(format!("{}\n", summary_line(&self.id, &attrs))),
        )?;
        drop(scan_logs);
        if let Some(e) = self.scope.failure.lock().unwrap().take() {
            return Err(ScanError::Staging(e));
        }
        self.staging.write_scan(&attrs)?;
        self.staging.set_state(if canceled {
            State::Canceled
        } else {
            State::Completed
        })?;
        // Apply: the staged notes become objects first, so the scan
        // can link them
        let notes = NoteStore::from(store);
        let mut note_shas = Vec::new();
        for dir in self.staging.staged_notes()? {
            let (_, sha) = notes.create_staged(&dir)?;
            note_shas.push(sha);
        }
        self.staging.write_notes_link(&note_shas)?;
        self.staging
            .write_notes_carried_link(&self.staging.staged_carried_notes()?)?;
        // Issues follow the notes they cite, so their evidence resolves
        let issues = IssueStore::from(store);
        let mut issue_shas = Vec::new();
        for dir in self.staging.staged_issues()? {
            let (_, sha) = issues.apply_staged(&dir)?;
            issue_shas.push(sha);
        }
        self.staging.write_issues_link(&issue_shas)?;
        let commit_sha = ScanStore::from(store).create(&self.id, &self.staging.scan_dir())?;
        self.staging.mark_applied()?;
        self.staging.remove()?;
        Ok(ScanOutcome {
            id: self.id,
            commit_sha,
            attrs,
        })
    }

    /// Dispatch the plan's tasks through the worker pool until every
    /// task is terminal or the scan is canceled, delivering task
    /// output as it arrives. Returns whether the scan was canceled; on
    /// cancellation every task without a terminal status is recorded
    /// `canceled` before returning.
    #[expect(
        clippy::indexing_slicing,
        reason = "task indices are plan-internal and bounded by construction"
    )]
    async fn run_tasks(
        &mut self,
        plan: &Plan,
        d: &mut Dispatch,
        logs: &mut Logs,
    ) -> Result<bool, ScanError> {
        let total = plan.tasks.len();
        let jobs = self.config.jobs.max(1);
        let mut running: JoinSet<(usize, Result<(), String>)> = JoinSet::new();
        loop {
            if self.cancel.is_cancelled() {
                break;
            }
            // Release ready tasks up to the pool size
            while running.len() < jobs {
                let Some(i) = d.ready.pop_front() else { break };
                self.start(plan, i, d, &mut running)?;
            }
            if d.status.iter().all(Option::is_some) {
                break;
            }
            let cancel = self.cancel;
            let output_rx = &mut self.output_rx;
            let joined = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                Some(joined) = running.join_next_with_id(), if !running.is_empty() => joined,
                Some(output) = output_rx.recv() => {
                    deliver(output, logs, &mut self.on_event)?;
                    continue;
                }
            };
            let (i, result) = match joined {
                Ok((id, (i, result))) => {
                    d.running.remove(&id);
                    (i, result)
                }
                Err(e) if e.is_cancelled() => continue,
                Err(e) => {
                    let i = d
                        .running
                        .remove(&e.id())
                        .expect("every spawned task is registered");
                    (i, Err(format!("task panicked: {e}")))
                }
            };
            // The task has returned; deliver what it sent before its
            // record closes
            self.drain_output(logs)?;
            let (status, error) = match result {
                Ok(()) => (TaskStatus::Completed, None),
                Err(message) => (TaskStatus::Failed, Some(message)),
            };
            self.finish(plan, i, status, error, d, logs)?;
        }
        let canceled = d.counts.completed + d.counts.failed + d.counts.skipped < total;
        if canceled {
            running.abort_all();
            while running.join_next().await.is_some() {}
            self.drain_output(logs)?;
            self.say(logs, ScanOutput::Err("scan canceled\n".into()))?;
            for i in 0..total {
                if d.status[i].is_none() {
                    self.finish(plan, i, TaskStatus::Canceled, None, d, logs)?;
                }
            }
        }
        Ok(canceled)
    }

    /// Record a task as started and hand it to a worker.
    #[expect(
        clippy::indexing_slicing,
        reason = "task indices are plan-internal and bounded by construction"
    )]
    fn start(
        &mut self,
        plan: &Plan,
        i: usize,
        d: &mut Dispatch,
        running: &mut JoinSet<(usize, Result<(), String>)>,
    ) -> Result<(), ScanError> {
        let t = &plan.tasks[i];
        let now = now_ms();
        d.started[i] = Some(now);
        self.staging.write_task(
            &t.scanner,
            &t.task,
            &task_attrs(TaskStatus::Started, Some(now), None),
        )?;
        (self.on_event)(Event::TaskStarted {
            scanner: t.scanner.clone(),
            task: t.task.clone(),
        });
        let unit = self.units[&t.scanner].clone();
        let mut ctx = self.scan_ctx.clone();
        ctx.params = unit.params.clone();
        let exec = TaskExec {
            unit,
            task: t.task.clone(),
            ctx,
            sink: OutputSink {
                scanner: t.scanner.clone(),
                task: t.task.clone(),
                tx: self.output_tx.clone(),
            },
            scope: LogScope {
                task: Some((t.scanner.clone(), t.task.clone())),
                ..self.scope.clone()
            },
        };
        let handle = running.spawn(async move { (i, exec.run().await) });
        d.running.insert(handle.id(), i);
        Ok(())
    }

    /// Record a task's terminal status, tell the sink, and release
    /// the downstream tasks it was holding.
    #[expect(
        clippy::indexing_slicing,
        reason = "task indices are plan-internal and bounded by construction"
    )]
    fn finish(
        &mut self,
        plan: &Plan,
        i: usize,
        status: TaskStatus,
        error: Option<String>,
        d: &mut Dispatch,
        logs: &mut Logs,
    ) -> Result<(), ScanError> {
        let t = &plan.tasks[i];
        let stopped = d.started[i].map(|_| now_ms());
        let attrs = task_attrs(status, d.started[i], stopped);
        self.staging.write_task(&t.scanner, &t.task, &attrs)?;
        match status {
            TaskStatus::Completed => d.counts.completed += 1,
            TaskStatus::Failed => d.counts.failed += 1,
            _ => {}
        }
        if let Some(message) = &error {
            self.say(
                logs,
                ScanOutput::Err(format!("task {} failed\n{message}", t.label())),
            )?;
        }
        (self.on_event)(Event::TaskFinished {
            scanner: t.scanner.clone(),
            task: t.task.clone(),
            status,
            error,
        });
        d.status[i] = Some(status);
        if status != TaskStatus::Canceled {
            for &down in &plan.downstream[i] {
                d.deps[down] -= 1;
                if d.deps[down] == 0 {
                    d.ready.push_back(down);
                }
            }
        }
        Ok(())
    }

    /// Deliver every output the tasks have sent so far.
    fn drain_output(&mut self, logs: &mut Logs) -> Result<(), ScanError> {
        while let Ok(output) = self.output_rx.try_recv() {
            deliver(output, logs, &mut self.on_event)?;
        }
        Ok(())
    }

    /// Record a line of the scan's own output, then hand it to the
    /// sink.
    fn say(&mut self, logs: &mut Logs, output: ScanOutput) -> Result<(), ScanError> {
        match &output {
            ScanOutput::Out(s) => logs.out(s)?,
            ScanOutput::Err(s) => logs.err(s)?,
        }
        (self.on_event)(Event::Scan(output));
        Ok(())
    }
}

/// The scan's closing line: `Scan <short id> completed: 3 tasks: 2
/// completed, 1 failed`, with skipped and canceled counts when
/// nonzero.
pub fn summary_line(id: &str, attrs: &ScanAttrs) -> String {
    let state = if attrs.canceled {
        "canceled"
    } else {
        "completed"
    };
    let counts = &attrs.tasks;
    let mut parts = vec![
        format!("{} completed", counts.completed),
        format!("{} failed", counts.failed),
    ];
    if counts.skipped > 0 {
        parts.push(format!("{} skipped", counts.skipped));
    }
    let canceled = counts.total - counts.completed - counts.failed - counts.skipped;
    if canceled > 0 {
        parts.push(format!("{canceled} canceled"));
    }
    format!(
        "Scan {} {state}: {} tasks: {}",
        short_uuid(id),
        counts.total,
        parts.join(", ")
    )
}

fn task_attrs(status: TaskStatus, started: Option<i64>, stopped: Option<i64>) -> TaskAttrs {
    TaskAttrs {
        status,
        started,
        stopped,
        worked_ms: None,
        skipped: None,
    }
}

/// Record one output in the scan's logs, then hand it to the sink.
fn deliver(
    output: TaskOutput,
    logs: &mut Logs,
    on_event: &mut impl FnMut(Event),
) -> Result<(), ScanError> {
    match &output.output {
        Output::Print(s) => logs.out(s)?,
        Output::Println(s) => {
            logs.out(s)?;
            logs.out("\n")?;
        }
        Output::Log { level, message } => {
            let origin = format!("{}:{}", output.scanner, output.task);
            logs.record(*level, &origin, message)?;
        }
        // A snapshot for a live view, not a record of the scan
        Output::Progress { .. } => {}
    }
    on_event(Event::Output(output));
    Ok(())
}

/// A scanner's compiled artifacts, shared by every worker running one
/// of its tasks.
#[derive(Clone)]
struct TaskUnit {
    rt: RuneArc<RuntimeContext>,
    unit: RuneArc<Unit>,
    sources: Arc<Sources>,
    params: Option<json::Value>,
}

/// Everything a worker needs to run one task: the scanner's
/// artifacts and the task-local scopes the task runs under.
struct TaskExec {
    unit: TaskUnit,
    task: String,
    ctx: ScanContext,
    sink: OutputSink,
    scope: LogScope,
}

impl TaskExec {
    /// Run the task under its scan context, output sink, and log
    /// scope.
    async fn run(self) -> Result<(), String> {
        let TaskExec {
            unit,
            task,
            ctx,
            sink,
            scope,
        } = self;
        LOG_SCOPE
            .scope(
                scope,
                SCAN_CTX.scope(ctx, OUTPUT_SINK.scope(sink, execute(&unit, &task))),
            )
            .await
    }
}

/// Run one task on a fresh VM.
async fn execute(scanner: &TaskUnit, task: &str) -> Result<(), String> {
    let vm = Vm::new(scanner.rt.clone(), scanner.unit.clone());
    let execution = vm
        .send_execute([task], ())
        .map_err(|e| vm_error(&e, &scanner.sources))?;
    let value = execution
        .complete()
        .await
        .map_err(|e| vm_error(&e, &scanner.sources))?;
    task_result(value, scanner, task)
}

/// Render a VM error as Rune does: the diagnostic with its source
/// excerpt, then a `Backtrace:` section listing every frame.
fn vm_error(e: &VmError, sources: &Sources) -> String {
    let mut buf = rune::termcolor::Buffer::no_color();
    e.emit(&mut buf, sources).unwrap();
    String::from_utf8(buf.into_inner()).unwrap()
}

/// Interpret a task's return value. A task returning unit or `Ok`
/// succeeded; `Err(e)` fails with a diagnostic naming `e` and
/// pointing at the task function.
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the runtime holds the only live handle"
)]
fn task_result(value: Value, scanner: &TaskUnit, task: &str) -> Result<(), String> {
    match rune::from_value::<Result<Value, Value>>(value) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(returned_error(&render_task_error(err), scanner, task)),
        // Not a Result: a task that returns unit or any other value
        Err(_) => Ok(()),
    }
}

/// The diagnostic for a task that returned `Err`: an `error:` line
/// with the value, labelled at the task function's first instruction
/// when the unit's debug info locates it.
fn returned_error(value: &str, scanner: &TaskUnit, task: &str) -> String {
    use codespan_reporting::diagnostic::{Diagnostic, Label};
    use codespan_reporting::term;

    let message = format!("task returned Err: {value}");
    let location = scanner.unit.debug_info().and_then(|info| {
        let hash = rune::Hash::type_hash([task]);
        let entry = info
            .functions_rev
            .iter()
            .filter(|(_, h)| **h == hash)
            .map(|(ip, _)| *ip)
            .min()?;
        // The entry instruction itself carries no debug entry; the
        // first recorded instruction at or after it is the body
        let ip = info.instructions.keys().filter(|ip| **ip >= entry).min()?;
        let inst = info.instruction_at(*ip)?;
        Some((inst.source_id, inst.span))
    });
    let mut diagnostic = Diagnostic::error().with_message(&message);
    if let Some((source_id, span)) = location {
        diagnostic = diagnostic.with_labels(vec![
            Label::primary(source_id, span.range()).with_message(format!("in task `{task}`")),
        ]);
    }
    let mut buf = rune::termcolor::Buffer::no_color();
    term::emit_to_write_style(
        &mut buf,
        &term::Config::default(),
        &*scanner.sources,
        &diagnostic,
    )
    .unwrap();
    String::from_utf8(buf.into_inner()).unwrap()
}

#[cfg(test)]
mod tests {
    use gage_registry::scanner::{ScannerDef, parse_scanner_file};
    use gage_store::DatasetStore;
    use tempfile::TempDir;

    use super::*;

    /// Write `source` as a scanner file and compile it. The directory
    /// guard is returned so the file outlives the compiled scanner.
    fn compile_source(source: &str) -> (TempDir, Result<CompiledScanner, Error>) {
        compile_source_with_params(source, None)
    }

    /// As [`compile_source`], with a `#{...}` params override applied
    /// over the scanner's declared defaults.
    fn compile_source_with_params(
        source: &str,
        params: Option<&str>,
    ) -> (TempDir, Result<CompiledScanner, Error>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scanner.rn");
        std::fs::write(&path, source).unwrap();
        let def = parse_scanner_file(&path).unwrap();
        let scanner = Scanner::from_spec(&def, params, "scanner.rn").unwrap();
        let compiled = compile(&scanner);
        (dir, compiled)
    }

    /// Compile `def` as its own explicit selection with default params.
    fn compile_def(def: &ScannerDef) -> Result<CompiledScanner, Error> {
        compile(&Scanner::from_spec(def, None, &def.name).unwrap())
    }

    /// A fresh store and staging root under one directory.
    fn open_store() -> (TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        (tmp, store)
    }

    /// Run `scanners` to completion, collecting every event.
    async fn run_all(
        store: &Store,
        root: &std::path::Path,
        scanners: &[CompiledScanner],
        cancel: &CancellationToken,
    ) -> (Result<ScanOutcome, ScanError>, Vec<Event>) {
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: root,
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
        };
        let outcome = scan(store, &config, scanners, cancel, |e| events.push(e)).await;
        (outcome, events)
    }

    fn outputs(events: &[Event]) -> Vec<&Output> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Output(o) => Some(&o.output),
                _ => None,
            })
            .collect()
    }

    const HELLO: &str = r#"
        pub const SCANNER = #{
            name: "hello",
            description: "Prints",
            tasks: #{ hello: #{} },
        };

        pub fn hello() {
            print!("a");
            println!("b {}", 1 + 1);
            print("c");
        }
    "#;

    const FAIL_THEN_RUN: &str = r#"
        pub const SCANNER = #{
            name: "fail",
            description: "Fails",
            tasks: #{ a: #{}, b: #{} },
        };

        pub fn a() {
            Err("boom")
        }

        pub fn b() {
            println!("b ran");
        }
    "#;

    #[tokio::test]
    async fn tasks_render_templates() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::Template;

            pub const SCANNER = #{
                name: "tpl",
                description: "Renders a template",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let t = Template::new("Review {{ session_id }}{% if extra %} ({{ extra }}){% endif %}")?;
                println!("{}", t.render(#{ session_id: "S1" })?);
                println!("{}", t.render(#{ session_id: "S2", extra: "more" })?);
                match Template::new("{% if") {
                    Err(gage::Error::Template(_)) => println!("template error"),
                    other => println!("unexpected: {other:?}"),
                }
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.unwrap().attrs.tasks.failed, 0, "{events:?}");
        assert_eq!(
            outputs(&events),
            [
                &Output::Println("Review S1".into()),
                &Output::Println("Review S2 (more)".into()),
                &Output::Println("template error".into()),
            ]
        );
    }

    #[tokio::test]
    async fn tasks_report_progress() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::Progress;

            pub const SCANNER = #{
                name: "prog",
                description: "Reports progress",
                tasks: #{ main: #{} },
            };

            pub fn main() {
                let p = Progress::new(3);
                p.tick();
                p.inc(2);
                for x in Progress::iter([10, 20].iter()) {
                    println!("{x}");
                }
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.unwrap().attrs.tasks.failed, 0, "{events:?}");
        assert_eq!(
            outputs(&events),
            [
                &Output::Progress { pos: 0, total: 3 },
                &Output::Progress { pos: 1, total: 3 },
                &Output::Progress { pos: 3, total: 3 },
                &Output::Progress { pos: 0, total: 2 },
                &Output::Progress { pos: 1, total: 2 },
                &Output::Println("10".into()),
                &Output::Progress { pos: 2, total: 2 },
                &Output::Println("20".into()),
            ]
        );
    }

    const PARAMS: &str = r#"
        use gage::params;

        pub const SCANNER = #{
            name: "params",
            description: "Prints params",
            params: #{ mode: #{ value: "roadmap" }, budget: #{ value: 0 } },
            tasks: #{ show: #{} },
        };

        pub fn show() {
            let p = params();
            println!("{} {}", p.mode, p.budget);
        }
    "#;

    #[tokio::test]
    async fn params_returns_the_declared_defaults() {
        let (_dir, compiled) = compile_source(PARAMS);
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        outcome.unwrap();
        assert_eq!(outputs(&events), [&Output::Println("roadmap 0".into())]);
    }

    #[tokio::test]
    async fn params_override_replaces_a_declared_default() {
        let (_dir, compiled) = compile_source_with_params(PARAMS, Some(r#"#{ mode: "query" }"#));
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        outcome.unwrap();
        assert_eq!(outputs(&events), [&Output::Println("query 0".into())]);
    }

    #[tokio::test]
    async fn params_is_an_empty_object_for_a_scanner_without_params() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::params;

            pub const SCANNER = #{
                name: "noparams",
                description: "Has no params",
                tasks: #{ show: #{} },
            };

            pub fn show() {
                println!("{}", params().is_empty());
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        outcome.unwrap();
        assert_eq!(outputs(&events), [&Output::Println("true".into())]);
    }

    #[tokio::test]
    async fn print_and_println_reach_the_sink_in_order() {
        let (_dir, compiled) = compile_source(HELLO);
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            outputs(&events),
            [
                &Output::Print("a".into()),
                &Output::Println("b 2".into()),
                &Output::Print("c".into()),
            ]
        );
        let outcome = outcome.unwrap();
        assert_eq!(
            outcome.attrs.tasks,
            TaskCounts {
                total: 1,
                completed: 1,
                failed: 0,
                skipped: 0,
            }
        );
        assert!(!outcome.attrs.canceled);
    }

    #[tokio::test]
    async fn async_task_returning_ok_completes() {
        let (_dir, compiled) = compile_source(
            r#"
            pub const SCANNER = #{
                name: "ok",
                description: "Returns Ok",
                tasks: #{ go: #{} },
            };

            pub async fn go() {
                println!("go");
                Ok(())
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outputs(&events), [&Output::Println("go".into())]);
        assert_eq!(outcome.unwrap().attrs.tasks.completed, 1);
    }

    /// The scan record lands in the store with one task record per
    /// task, the failed task's message in `logs/err`, and staging
    /// removed once applied.
    #[tokio::test]
    async fn scan_records_every_task_and_removes_staging() {
        let (_dir, compiled) = compile_source(FAIL_THEN_RUN);
        let (tmp, store) = open_store();
        let root = tmp.path().join("staging");
        let (outcome, events) = run_all(
            &store,
            &root,
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        let failure = match &events[2] {
            Event::TaskFinished {
                error: Some(message),
                ..
            } => message.clone(),
            other => panic!("expected the failure of task a, got {other:?}"),
        };
        let notice = format!("task fail:a failed\n{failure}");
        let summary = format!("{}\n", summary_line(&outcome.id, &outcome.attrs));
        assert_eq!(
            events,
            [
                Event::TaskStarted {
                    scanner: "fail".into(),
                    task: "a".into(),
                },
                Event::Scan(ScanOutput::Err(notice.clone())),
                Event::TaskFinished {
                    scanner: "fail".into(),
                    task: "a".into(),
                    status: TaskStatus::Failed,
                    error: Some(failure.clone()),
                },
                Event::TaskStarted {
                    scanner: "fail".into(),
                    task: "b".into(),
                },
                Event::Output(TaskOutput {
                    scanner: "fail".into(),
                    task: "b".into(),
                    output: Output::Println("b ran".into()),
                }),
                Event::TaskFinished {
                    scanner: "fail".into(),
                    task: "b".into(),
                    status: TaskStatus::Completed,
                    error: None,
                },
                Event::Scan(ScanOutput::Out(summary.clone())),
            ]
        );
        assert_eq!(
            summary,
            format!(
                "Scan {} completed: 2 tasks: 1 completed, 1 failed\n",
                short_uuid(&outcome.id)
            )
        );
        assert!(
            failure.starts_with("error: task returned Err: boom\n"),
            "{failure}"
        );
        assert!(
            failure.contains("Err(\"boom\")") && failure.contains("in task `a`"),
            "the diagnostic points into the task function:\n{failure}"
        );
        assert_eq!(
            outcome.attrs.tasks,
            TaskCounts {
                total: 2,
                completed: 1,
                failed: 1,
                skipped: 0,
            }
        );
        assert!(outcome.attrs.started <= outcome.attrs.stopped);

        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert_eq!(record.commit_sha, outcome.commit_sha);
        assert_eq!(record.content.attrs, outcome.attrs);
        assert_eq!(record.content.attrs.runtime, "gage test-version");
        // `records` is present too when another test has installed
        // the process-wide records layer
        let logs: Vec<&str> = record
            .content
            .logs
            .iter()
            .map(String::as_str)
            .filter(|n| *n != "records")
            .collect();
        assert_eq!(logs, ["err", "out"]);
        let scans = ScanStore::from(&store);
        assert_eq!(
            scans.scan_log(&outcome.commit_sha, "out").unwrap(),
            Some(format!("b ran\n{summary}").into_bytes()),
            "the scan's out holds what the terminal showed"
        );
        assert_eq!(
            scans.scan_log(&outcome.commit_sha, "err").unwrap(),
            Some(notice.into_bytes())
        );
        assert_eq!(
            record.content.scanners,
            std::collections::BTreeMap::from([(
                "fail".to_string(),
                vec!["scanner.rn".to_string()]
            )])
        );
        assert_eq!(
            ScanStore::from(&store)
                .source_file(&outcome.commit_sha, "fail", "scanner.rn")
                .unwrap()
                .as_deref(),
            Some(FAIL_THEN_RUN.as_bytes()),
            "the stored source is the file byte for byte"
        );
        let [a, b] = record.content.tasks.as_slice() else {
            panic!("two task records: {:?}", record.content.tasks);
        };
        assert_eq!(a.task, "a");
        assert_eq!(a.attrs.status, TaskStatus::Failed);
        assert!(a.attrs.started.is_some() && a.attrs.stopped.is_some());
        assert_eq!(b.task, "b");
        assert_eq!(b.attrs.status, TaskStatus::Completed);

        assert!(
            !root.join(&outcome.id).exists(),
            "staging is removed after apply"
        );
    }

    /// Print output lands in the scan's `logs/out` in delivery order,
    /// and log records in `logs/records` with the task as origin.
    #[tokio::test]
    async fn task_output_and_records_are_stored_under_the_scan_logs() {
        let (_dir, compiled) = compile_source(
            r#"
            pub const SCANNER = #{
                name: "logs",
                description: "Logs",
                tasks: #{ loud: #{}, quiet: #{} },
            };

            pub fn loud() {
                print!("a");
                println!("b");
                log::info!("count {}", 3);
                log::warn!("careful");
            }

            pub fn quiet() {}
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        assert!(events.contains(&Event::Output(TaskOutput {
            scanner: "logs".into(),
            task: "loud".into(),
            output: Output::Log {
                level: gage_runtime2::Level::Warn,
                message: "careful".into(),
            },
        })));
        let scans = ScanStore::from(&store);
        let out = scans.scan_log(&outcome.commit_sha, "out").unwrap().unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("ab\n{}\n", summary_line(&outcome.id, &outcome.attrs))
        );
        let records = scans
            .scan_log(&outcome.commit_sha, "records")
            .unwrap()
            .unwrap();
        let records = String::from_utf8(records).unwrap();
        // Runtime records share the file when another test has
        // installed the process-wide records layer
        let lines: Vec<&str> = records
            .lines()
            .filter(|l| l.contains(" logs:loud: "))
            .collect();
        assert_eq!(lines.len(), 2, "{records}");
        assert!(lines[0].ends_with("Z INFO logs:loud: count 3"), "{records}");
        assert!(lines[1].ends_with("Z WARN logs:loud: careful"), "{records}");
    }

    /// A scanner's includes are stored beside it under their literal
    /// names, and the task sees the included text.
    #[tokio::test]
    async fn included_files_are_stored_as_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shared")).unwrap();
        std::fs::write(dir.path().join("shared/msg.txt"), "from shared\n").unwrap();
        let scanner_dir = dir.path().join("s");
        std::fs::create_dir_all(&scanner_dir).unwrap();
        std::fs::write(scanner_dir.join("local.txt"), "from local\n").unwrap();
        let path = scanner_dir.join("scanner.rn");
        std::fs::write(
            &path,
            r#"
            pub const SCANNER = #{
                name: "inc",
                description: "Includes",
                tasks: #{ go: #{} },
            };

            const LOCAL = include_str!("local.txt");
            const SHARED = include_str!("../shared/msg.txt");

            pub fn go() {
                print(LOCAL);
                print(SHARED);
            }
            "#,
        )
        .unwrap();
        let compiled = compile_def(&parse_scanner_file(&path).unwrap()).unwrap();
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(
            outputs(&events),
            [
                &Output::Print("from local\n".into()),
                &Output::Print("from shared\n".into()),
            ]
        );
        let scans = ScanStore::from(&store);
        let record = scans.get(&outcome.id).unwrap();
        assert_eq!(
            record.content.scanners["inc"],
            ["..%2Fshared%2Fmsg.txt", "local.txt", "scanner.rn"]
        );
        assert_eq!(
            scans
                .source_file(&outcome.commit_sha, "inc", "..%2Fshared%2Fmsg.txt")
                .unwrap()
                .as_deref(),
            Some(b"from shared\n".as_slice())
        );
    }

    #[tokio::test]
    async fn vm_error_is_reported_with_its_location() {
        let (_dir, compiled) = compile_source(
            r#"
            pub const SCANNER = #{
                name: "panic",
                description: "Panics",
                tasks: #{ go: #{} },
            };

            pub fn go() {
                let v = [];
                v[3]
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.unwrap().attrs.tasks.failed, 1);
        let Some(Event::TaskFinished {
            error: Some(message),
            ..
        }) = events
            .iter()
            .find(|e| matches!(e, Event::TaskFinished { .. }))
        else {
            panic!("expected a failure, got {events:?}");
        };
        assert!(message.starts_with("error: "), "{message}");
        assert!(message.contains("v[3]"), "{message}");
        assert!(
            message.contains("Backtrace:"),
            "the VM error carries its backtrace:\n{message}"
        );
    }

    /// A token cancelled before the run starts marks every task
    /// `canceled` without a start time and applies the scan as
    /// canceled.
    #[tokio::test]
    async fn cancelled_token_marks_every_task_canceled() {
        let (_dir, compiled) = compile_source(FAIL_THEN_RUN);
        let (tmp, store) = open_store();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &cancel,
        )
        .await;
        let outcome = outcome.unwrap();
        assert!(outcome.attrs.canceled);
        assert_eq!(
            outcome.attrs.tasks.completed + outcome.attrs.tasks.failed,
            0
        );
        let finished: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::TaskFinished { .. }))
            .collect();
        assert_eq!(finished.len(), 2);
        assert!(finished.iter().all(|e| matches!(
            e,
            Event::TaskFinished {
                status: TaskStatus::Canceled,
                ..
            }
        )));
        assert_eq!(
            events.first(),
            Some(&Event::Scan(ScanOutput::Err("scan canceled\n".into()))),
            "the cancel notice is given once, first"
        );
        assert_eq!(
            events.last(),
            Some(&Event::Scan(ScanOutput::Out(format!(
                "Scan {} canceled: 2 tasks: 0 completed, 0 failed, 2 canceled\n",
                short_uuid(&outcome.id)
            ))))
        );
        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert!(
            record
                .content
                .tasks
                .iter()
                .all(|t| { t.attrs.status == TaskStatus::Canceled && t.attrs.started.is_none() })
        );
    }

    /// Runtime `tracing` events go to the scan's `records` with the
    /// Rust target after the level, and carry the running task as an
    /// attribute when raised inside one.
    #[tokio::test]
    async fn runtime_records_name_the_running_task() {
        use std::sync::Once;
        use tracing_subscriber::layer::SubscriberExt;

        // Process-wide, once: a thread-scoped subscriber would miss
        // callsites other tests hit first with no subscriber, whose
        // cached interest stays disabled. The layer drops events
        // outside a scan scope, so other tests are unaffected.
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(trace::layer()),
            )
            .unwrap();
        });

        // The runtime's `write_note` raises a debug record from inside
        // the task; the sink runs on the scan loop, outside any task
        let (_dir, compiled) = compile_source(
            r#"
            pub const SCANNER = #{
                name: "notes",
                description: "Writes a note",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                gage::write_note("greeting", "hello").await?;
            }
            "#,
        );
        let (tmp, store) = open_store();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |event| {
                if let Event::TaskStarted { .. } = event {
                    tracing::warn!("outside the task")
                }
            },
        )
        .await
        .unwrap();

        let scans = ScanStore::from(&store);
        let records = String::from_utf8(
            scans
                .scan_log(&outcome.commit_sha, "records")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            records.contains(" INFO gage_scan2: scan ")
                && records.contains(" started with 1 tasks\n"),
            "{records}"
        );
        assert!(
            records.contains(" WARN gage_scan2::tests: outside the task\n"),
            "{records}"
        );
        assert!(
            records
                .lines()
                .any(|l| l.contains(" DEBUG gage_runtime2::note: write_note")
                    && l.ends_with(" task=notes:main")),
            "{records}"
        );
    }

    #[tokio::test]
    async fn duplicate_scanner_names_are_rejected_before_staging() {
        let (_a, first) = compile_source(HELLO);
        let (_b, second) = compile_source(HELLO);
        let (tmp, store) = open_store();
        let root = tmp.path().join("staging");
        let (outcome, _) = run_all(
            &store,
            &root,
            &[first.unwrap(), second.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, Err(ScanError::DuplicateScanner(ref n)) if n == "hello"));
        assert!(!root.exists());
    }

    #[test]
    fn declared_task_without_a_function_fails_to_compile() {
        let (_dir, compiled) = compile_source(
            r#"
            pub const SCANNER = #{
                name: "missing",
                description: "Missing",
                tasks: #{ nope: #{} },
            };
            "#,
        );
        let err = compiled.err().unwrap();
        assert!(
            matches!(&err, Error::MissingTask { scanner, task } if scanner == "missing" && task == "nope"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn scan_links_the_dataset_commit() {
        let (_dir, compiled) = compile_source(HELLO);
        let (tmp, store) = open_store();
        let datasets = DatasetStore::from(&store);
        let dataset_sha = datasets
            .get(&datasets.create().unwrap())
            .unwrap()
            .commit_sha;
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(
            store.read_commit(&outcome.commit_sha).unwrap().parents,
            [dataset_sha.clone()]
        );
        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert_eq!(record.content.dataset, Some(dataset_sha));
    }

    /// `scan()` gives a task the scan id and its dataset, and the
    /// dataset's sessions; a scan without a dataset has none.
    #[tokio::test]
    async fn tasks_read_the_scan_and_its_dataset() {
        const SCANNER: &str = r#"
            use gage::scan;

            pub const SCANNER = #{
                name: "ctx",
                description: "Scan context",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let s = scan();
                println!("{}", s.id);
                match s.dataset {
                    Some(d) => println!("dataset {}", d.id),
                    None => println!("no dataset"),
                }
                println!("{} sessions", scan().sessions().await.len());
            }
        "#;
        let (tmp, store) = open_store();
        let datasets = DatasetStore::from(&store);
        let dataset_id = datasets.create().unwrap();
        let dataset_sha = datasets.get(&dataset_id).unwrap().commit_sha;

        let (_dir, compiled) = compile_source(SCANNER);
        let compiled = compiled.unwrap();
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            std::slice::from_ref(&compiled),
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(
            outputs(&events),
            [
                &Output::Println(outcome.id.clone()),
                &Output::Println(format!("dataset {dataset_id}")),
                &Output::Println("0 sessions".into()),
            ]
        );

        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(
            outputs(&events),
            [
                &Output::Println(outcome.id.clone()),
                &Output::Println("no dataset".into()),
                &Output::Println("0 sessions".into()),
            ]
        );
    }

    /// A dataset holding one `claude` session seeded from `jsonl`.
    /// Returns the dataset id, its commit, and the session's Gage id.
    fn seeded_dataset(
        root: &std::path::Path,
        store: &Store,
        jsonl: &str,
    ) -> (String, String, String) {
        use gage_session::Driver;
        use gage_store::SessionSpec;

        let claude = root.join("claude");
        let native_id = "11111111-2222-3333-4444-555555555555";
        let dir = claude.join("projects").join("-home-alice-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{native_id}.jsonl")), jsonl).unwrap();
        let driver = gage_claude::driver::ClaudeDriver::new();
        let source = driver
            .open_source(&format!("claude:{}", claude.display()))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();

        let datasets = DatasetStore::from(store);
        let dataset_id = datasets.create().unwrap();
        let outcomes = datasets
            .sessions_add(
                &dataset_id,
                vec![SessionSpec {
                    driver: &driver,
                    session: &mut *native,
                }],
            )
            .unwrap();
        let dataset_sha = datasets.get(&dataset_id).unwrap().commit_sha;
        (dataset_id, dataset_sha, outcomes[0].id.clone())
    }

    /// `messages()` and `entries()` read a member session through
    /// its driver at the commit the dataset links, scoped to that
    /// session, with `.type(spec)`, `.latest_first()`, `.lines()`, and
    /// `.limit()` applied.
    #[tokio::test]
    async fn tasks_read_session_messages_and_entries() {
        const SCANNER: &str = r#"
            use gage::scan;

            pub const SCANNER = #{
                name: "rows",
                description: "Session rows",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                for s in scan().sessions().await {
                    for m in s.messages().await? {
                        println!("{}/{}: {}", m.type, m.subtype.unwrap_or("-"), m.text);
                    }
                    for m in s.messages().type("assistant").latest_first().await? {
                        println!("latest assistant: {}", m.text);
                    }
                    for m in s.messages().lines(2, 3).await? {
                        println!("lines 2-3: {}", m.text);
                    }
                    for m in s.messages().latest_first().limit(1).await? {
                        println!("limit 1: {}", m.text);
                    }
                    println!("{} entries", s.entries().await?.len());
                    println!("{} entries limit 2", s.entries().limit(2).await?.len());
                    for e in s.entries().type("summary").await? {
                        println!("entry {}: {}", e.line, e.type);
                    }
                    match s.messages().type(#{}).await {
                        Err(gage::Error::Args(msg)) => println!("args error: {msg}"),
                        other => println!("unexpected: {other:?}"),
                    }
                }
                Ok(())
            }
        "#;
        const SESSION: &str = concat!(
            r#"{"type":"summary","summary":"A chat"}"#,
            "\n",
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"hi there"}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a2","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"anything else?"}]}}"#,
            "\n",
        );

        let (tmp, store) = open_store();
        let (_, dataset_sha, _) = seeded_dataset(tmp.path(), &store, SESSION);

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
        assert_eq!(
            outputs(&events),
            [
                &Output::Println("user/text: hello".into()),
                &Output::Println("assistant/text: hi there".into()),
                &Output::Println("assistant/text: anything else?".into()),
                &Output::Println("latest assistant: anything else?".into()),
                &Output::Println("latest assistant: hi there".into()),
                &Output::Println("lines 2-3: hello".into()),
                &Output::Println("lines 2-3: hi there".into()),
                &Output::Println("limit 1: anything else?".into()),
                &Output::Println("4 entries".into()),
                &Output::Println("2 entries limit 2".into()),
                &Output::Println("entry 1: summary".into()),
                &Output::Println("args error: `.type()` object must name at least one type".into()),
            ]
        );
    }

    /// `write_note` stages a note the apply creates and the scan links:
    /// the author is the task, `attrs.scan` is the scan, a session
    /// target pins the member commit, and bad lines are the scanner's
    /// error.
    #[tokio::test]
    async fn tasks_write_notes_that_the_scan_links() {
        use gage_store::NoteStore;

        const SCANNER: &str = r#"
            use gage::{scan, write_note};

            pub const SCANNER = #{
                name: "notes",
                description: "Notes",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                for s in scan().sessions().await {
                    let n = write_note("thinking.empty", true)
                        .for_session_line(s.id, "2")
                        .metadata(#{ model: "m" })
                        .await?;
                    println!("{} {} {:?}", n.name, n.author, n.target);
                    let n = write_note("comment", "whole")
                        .for_session_lines(s.id, "")
                        .await?;
                    println!("{:?}", n.target);
                    let n = write_note("comment", "ranged")
                        .for_session_range(s.id, 1, 3)
                        .await?;
                    println!("{:?}", n.target);
                    let n = write_note("comment", "by-session").for_session(s).await?;
                    println!("{:?} {}", n.target, s.id);
                    match write_note("bad", 1).for_session_line(s.id, 0).await {
                        Err(gage::Error::Args(m)) => println!("args: {m}"),
                        other => println!("unexpected: {other:?}"),
                    }
                }
                let n = write_note("scan.fact", #{ ok: true }).await?;
                println!("{:?} {}", n.target, n.metadata.len());
                Ok(())
            }
        "#;
        const SESSION: &str = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","model":"m","content":[{"type":"thinking","thinking":""}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a2","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
        );

        let (tmp, store) = open_store();
        let (dataset_id, dataset_sha, session_id) = seeded_dataset(tmp.path(), &store, SESSION);
        let member_sha = DatasetStore::from(&store)
            .sessions_at(&dataset_sha)
            .unwrap()[0]
            .commit_sha
            .clone();

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
        assert_eq!(
            outputs(&events),
            [
                &Output::Println(format!(
                    "thinking.empty task:notes:main Some(\"session:{session_id}#2\")"
                )),
                &Output::Println(format!("Some(\"session:{session_id}\")")),
                &Output::Println(format!("Some(\"session:{session_id}#1-3\")")),
                &Output::Println(format!("Some(\"session:{session_id}\") {session_id}")),
                &Output::Println("args: line must be 1 or greater, got 0".into()),
                &Output::Println("None 0".into()),
            ]
        );

        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert_eq!(record.content.notes.len(), 5);
        let parents = store.read_commit(&outcome.commit_sha).unwrap().parents;
        for sha in &record.content.notes {
            assert!(parents.contains(sha), "note {sha} is a scan parent");
        }
        // notes.link is in id order, not write order
        let notes = NoteStore::from(&store);
        let first = record
            .content
            .notes
            .iter()
            .map(|sha| notes.at_commit(sha).unwrap())
            .find(|n| n.name == "thinking.empty")
            .expect("the scanner wrote thinking.empty");
        assert_eq!(first.scan.as_deref(), Some(outcome.id.as_str()));
        assert_eq!(first.author, "task:notes:main");
        assert_eq!(
            first.targets,
            [member_sha],
            "the note links the member commit the scan read"
        );
        assert_eq!(first.metadata, Some(serde_json::json!({"model": "m"})));
        assert!(
            !tmp.path().join("staging").join(&outcome.id).exists(),
            "staging is removed after apply"
        );

        // The scan and its relations are queryable through the tables
        let ctx = gage_query2::ContextBuilder::new(Some(Arc::new(Mutex::new(
            Store::open(store.path()).unwrap(),
        ))))
        .build()
        .await;
        let count = |sql: String| {
            let ctx = ctx.clone();
            async move {
                let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
                batches[0]
                    .column(0)
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                    .unwrap()
                    .value(0)
            }
        };
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM scan_note WHERE scan_id = '{}'",
                outcome.id
            ))
            .await,
            5
        );
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM scan_session WHERE scan_id = '{}'",
                outcome.id
            ))
            .await,
            1
        );
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM scan WHERE id = '{}' AND dataset = '{}'",
                outcome.id, dataset_id
            ))
            .await,
            1
        );
    }

    #[tokio::test]
    async fn tasks_write_issues_that_cite_staged_notes_and_the_scan_links() {
        use gage_store::{IssueInput, IssueStatus, IssueStore, NoteStore};

        const SCANNER: &str = r###"
            use gage::{issues, scan, write_issue, write_note};

            pub const SCANNER = #{
                name: "issues",
                description: "Issues",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let before = issues().await?;
                println!("before {} {}", before.len(), before[0].status);
                let note = None;
                for s in scan().sessions().await {
                    note = Some(write_note("finding.code", "retry loop")
                        .for_session_line(s.id, 2)
                        .await?);
                }
                let note = note.unwrap();
                let i = write_issue("findings", "Retry loop", "## Summary\n\nRetries.")
                    .evidence(note)
                    .pending()
                    .await?;
                println!("{} {} {} {:?} {:?}", i.name, i.status, i.author, i.evidence, i.description);
                let j = write_issue("session-retention", "Retention unset", "")
                    .evidence([note.id, note.id])
                    .await?;
                println!("{} {} {:?} {:?}", j.name, j.status, j.evidence.len(), j.description);
                match write_issue("bad", "t", "d").evidence("nosuchnote").await {
                    Err(gage::Error::Args(m)) => println!("args: {m}"),
                    other => println!("unexpected: {other:?}"),
                }
                match write_issue("bad", "t", "d").evidence(1).await {
                    Err(gage::Error::Args(m)) => println!("args: {m}"),
                    other => println!("unexpected: {other:?}"),
                }
                let all = issues().await?;
                let pending = issues().status("pending").await?;
                let named = issues().name(["findings", "prior"]).status(["open", "pending"]).await?;
                println!("after {} {} {}", all.len(), pending.len(), named.len());
                match issues().status("bogus").await {
                    Err(gage::Error::Args(m)) => println!("args: {m}"),
                    other => println!("unexpected: {other:?}"),
                }
                Ok(())
            }
        "###;
        const SESSION: &str = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
        );

        let (tmp, store) = open_store();
        let (_dataset_id, dataset_sha, session_id) = seeded_dataset(tmp.path(), &store, SESSION);
        // An issue already in the store is visible to the task
        let prior = IssueStore::from(&store)
            .create(IssueInput {
                name: "prior",
                title: "Prior",
                description: None,
                author: "user:t",
                status: IssueStatus::Open,
                evidence: &[],
                replace_key: None,
            })
            .unwrap();

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");

        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert_eq!(record.content.notes.len(), 1);
        assert_eq!(record.content.issues.len(), 2);
        let parents = store.read_commit(&outcome.commit_sha).unwrap().parents;
        for sha in &record.content.issues {
            assert!(parents.contains(sha), "issue {sha} is a scan parent");
        }
        let note_sha = record.content.notes[0].clone();
        let note_id = NoteStore::from(&store).at_commit(&note_sha).unwrap().id;
        let issues = IssueStore::from(&store);
        let mut written: Vec<_> = record
            .content
            .issues
            .iter()
            .map(|sha| issues.at_commit(sha).unwrap())
            .collect();
        written.sort_by(|a, b| a.name.cmp(&b.name));
        let findings = &written[0];
        assert_eq!(findings.name, "findings");
        assert_eq!(findings.status, IssueStatus::Pending);
        assert_eq!(findings.author, "task:issues:main");
        assert_eq!(findings.scan.as_deref(), Some(outcome.id.as_str()));
        assert_eq!(
            findings.evidence,
            [note_sha.clone()],
            "the issue links the staged note's commit"
        );
        assert_eq!(
            findings.description.as_deref(),
            Some("## Summary\n\nRetries.")
        );
        assert_eq!(findings.changes.len(), 1);
        let retention = &written[1];
        assert_eq!(retention.status, IssueStatus::Open);
        assert_eq!(
            retention.evidence,
            [note_sha.clone()],
            "a repeated citation links once"
        );
        assert_eq!(
            retention.description, None,
            "an empty description writes no file"
        );

        assert_eq!(
            outputs(&events),
            [
                &Output::Println("before 1 open".into()),
                &Output::Println(format!(
                    "findings pending task:issues:main [\"{note_id}\"] Some(\"## Summary\\n\\nRetries.\")"
                )),
                &Output::Println(format!("session-retention open 1 None")),
                &Output::Println("args: write_issue evidence: object not found: nosuchnote".into()),
                &Output::Println(
                    "args: evidence must be a note id, a Note, or a list of either".into()
                ),
                &Output::Println("after 3 1 2".into()),
                &Output::Println(
                    "args: issues status: invalid issue input: unknown issue status \"bogus\""
                        .into()
                ),
            ]
        );
        assert_eq!(
            issues.get(&prior).unwrap().status,
            IssueStatus::Open,
            "the prior issue is untouched"
        );

        // The scan's issues and their evidence are queryable
        let ctx = gage_query2::ContextBuilder::new(Some(Arc::new(Mutex::new(
            Store::open(store.path()).unwrap(),
        ))))
        .build()
        .await;
        let count = |sql: String| {
            let ctx = ctx.clone();
            async move {
                let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
                batches[0]
                    .column(0)
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                    .unwrap()
                    .value(0)
            }
        };
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM scan_issue WHERE scan_id = '{}'",
                outcome.id
            ))
            .await,
            2
        );
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM session_issue WHERE session_id = '{session_id}'"
            ))
            .await,
            2
        );
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM issue WHERE scan = '{}' AND status = 'pending'",
                outcome.id
            ))
            .await,
            1
        );
    }

    /// `replace_named()` and `replace_keyed(key)` store a replace key.
    /// A later scan's write under the same key is a new commit of the
    /// live issue, with the write's whole state, and the scan links
    /// that commit. A second write in one scan replaces the staged one.
    #[tokio::test]
    async fn issues_written_under_a_replace_key_replace_the_live_issue() {
        use gage_store::{ChangeEvent, IssueStatus, IssueStore};

        const SCANNER: &str = r###"
            use gage::{scan, write_issue, write_note};

            pub const SCANNER = #{
                name: "replace",
                description: "Replace",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let note = None;
                for s in scan().sessions().await {
                    note = Some(write_note("empty-thinking", true)
                        .for_session_line(s.id, 1)
                        .await?);
                }
                let note = note.unwrap();
                let first = write_issue("hidden-thinking", "Hidden", "first")
                    .replace_named()
                    .evidence(note)
                    .await?;
                let i = write_issue("hidden-thinking", "Hidden", "second")
                    .replace_named()
                    .evidence(note)
                    .await?;
                println!("{} {} {:?} {}", i.id == first.id, i.status, i.replace_key, i.evidence.len());
                let k = write_issue("per-session", "Per session", "")
                    .replace_keyed(("per-session", 7))
                    .await?;
                println!("{:?}", k.replace_key);
                Ok(())
            }
        "###;
        const SESSION: &str = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
        );

        let (tmp, store) = open_store();
        let (_, dataset_sha, _) = seeded_dataset(tmp.path(), &store, SESSION);
        let (_dir, compiled) = compile_source(SCANNER);
        let compiled = compiled.unwrap();
        let run = |n: u32| {
            let store = &store;
            let compiled = &compiled;
            let root = tmp.path().join(format!("staging{n}"));
            let dataset_sha = dataset_sha.clone();
            async move {
                let mut events = Vec::new();
                let config = ScanConfig {
                    staging_root: &root,
                    gage_version: "test-version",
                    dataset: Some(&dataset_sha),
                    jobs: 1,
                };
                let outcome = scan(
                    store,
                    &config,
                    std::slice::from_ref(compiled),
                    &CancellationToken::new(),
                    |e| events.push(e),
                )
                .await
                .unwrap();
                assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
                assert_eq!(
                    outputs(&events),
                    [
                        &Output::Println("true open Some(\"hidden-thinking\") 1".into()),
                        &Output::Println("Some(\"per-session:7\")".into()),
                    ]
                );
                outcome
            }
        };

        let first = run(1).await;
        let issues = IssueStore::from(&store);
        let record = ScanStore::from(&store).get(&first.id).unwrap();
        assert_eq!(record.content.issues.len(), 2);
        let mut written: Vec<_> = record
            .content
            .issues
            .iter()
            .map(|sha| issues.at_commit(sha).unwrap())
            .collect();
        written.sort_by(|a, b| a.name.cmp(&b.name));
        let hidden = written[0].clone();
        assert_eq!(hidden.name, "hidden-thinking");
        assert_eq!(hidden.replace_key.as_deref(), Some("hidden-thinking"));
        assert_eq!(
            hidden.description.as_deref(),
            Some("second"),
            "the second write in the scan replaced the first staged one"
        );
        assert_eq!(hidden.changes.len(), 1);
        assert_eq!(hidden.evidence, [record.content.notes[0].clone()]);
        let per_session = written[1].clone();
        assert_eq!(per_session.replace_key.as_deref(), Some("per-session:7"));

        issues
            .set_status(&hidden.id, IssueStatus::Closed, None, "user:t", None)
            .unwrap();

        let second = run(2).await;
        let record = ScanStore::from(&store).get(&second.id).unwrap();
        assert_eq!(record.content.issues.len(), 2);
        let parents = store.read_commit(&second.commit_sha).unwrap().parents;
        for sha in &record.content.issues {
            assert!(parents.contains(sha), "issue {sha} is a scan parent");
        }
        let mut written: Vec<_> = record
            .content
            .issues
            .iter()
            .map(|sha| issues.at_commit(sha).unwrap())
            .collect();
        written.sort_by(|a, b| a.name.cmp(&b.name));
        let replaced = &written[0];
        assert_eq!(replaced.id, hidden.id, "the same issue");
        assert_ne!(replaced.commit_sha, hidden.commit_sha, "a new commit of it");
        assert_eq!(replaced.status, IssueStatus::Open);
        assert_eq!(replaced.scan.as_deref(), Some(second.id.as_str()));
        assert_eq!(
            replaced.evidence,
            [record.content.notes[0].clone()],
            "the evidence is the second scan's note alone"
        );
        assert_eq!(
            replaced
                .changes
                .iter()
                .map(|c| (c.event, c.from_status, c.to_status))
                .collect::<Vec<_>>(),
            [
                (ChangeEvent::Create, None, Some(IssueStatus::Open)),
                (
                    ChangeEvent::Status,
                    Some(IssueStatus::Open),
                    Some(IssueStatus::Closed)
                ),
                (
                    ChangeEvent::Status,
                    Some(IssueStatus::Closed),
                    Some(IssueStatus::Open)
                ),
            ]
        );
        assert_eq!(
            issues.get(&hidden.id).unwrap().commit_sha,
            replaced.commit_sha
        );
        let per_session_again = &written[1];
        assert_eq!(per_session_again.id, per_session.id);
        assert_eq!(
            per_session_again.changes.last().unwrap().event,
            ChangeEvent::Edit
        );
        assert_eq!(
            issues.query().name("hidden-thinking").count().unwrap(),
            1,
            "no second issue under the name"
        );
    }

    /// `scan().attachments()` reads the dataset's attachments at the
    /// commit the scan links; `scan().attachment(name)` yields one or
    /// `None`; `file(key)` reads content and `None` for an absent
    /// key.
    #[tokio::test]
    async fn attachments_are_read_from_the_scanned_dataset_commit() {
        use gage_store::{AttachmentSpec, AttachmentStore};

        const SCANNER: &str = r###"
            use gage::scan;

            pub const SCANNER = #{
                name: "attach",
                description: "Reads attachments",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let all = scan().attachments().await;
                println!("{}", all.len());
                let a = scan().attachment("cfg").await.unwrap();
                println!("{} {:?}", a.name, a.files().await);
                let f = a.file("settings.json").await.unwrap();
                println!("{:?}", f.json()?.get("cleanupPeriodDays"));
                println!("{}", a.file("missing.json").await.is_none());
                println!("{}", scan().attachment("nope").await.is_none());
                Ok(())
            }
        "###;

        let (tmp, store) = open_store();
        let root = tmp.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("settings.json"), "{\"cleanupPeriodDays\": 365}").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "rules").unwrap();
        let attachments = AttachmentStore::from(&store);
        let datasets = DatasetStore::from(&store);
        let pats = ["settings.json".to_string()];
        for name in ["cfg", "other"] {
            let added = attachments
                .add(&AttachmentSpec {
                    name,
                    root: &root,
                    includes: &pats,
                    excludes: &[],
                })
                .unwrap();
            let dataset = datasets.create().unwrap();
            datasets.attachments_link(&dataset, &[added.id]).unwrap();
        }
        // One dataset holds both; scan that one
        let dataset = datasets.create().unwrap();
        let ids: Vec<String> = ["cfg", "other"]
            .iter()
            .map(|n| attachments.get_by_name(n).unwrap().id)
            .collect();
        datasets.attachments_link(&dataset, &ids).unwrap();
        let dataset_sha = datasets.get(&dataset).unwrap().commit_sha;

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
        assert_eq!(
            outputs(&events),
            [
                &Output::Println("2".into()),
                &Output::Println("cfg [\"settings.json\"]".into()),
                &Output::Println("Some(365)".into()),
                &Output::Println("true".into()),
                &Output::Println("true".into()),
            ]
        );
    }

    /// `keep_named()` writes once: a second scan finds the issue live
    /// in the store, in any status, and returns it without a write;
    /// a second write in one scan returns the staged one.
    #[tokio::test]
    async fn issues_written_under_a_keep_key_are_not_rewritten() {
        use gage_store::{IssueStatus, IssueStore};

        const SCANNER: &str = r###"
            use gage::write_issue;

            pub const SCANNER = #{
                name: "keep",
                description: "Keep",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let first = write_issue("retention", "Retention", "first")
                    .keep_named()
                    .await?;
                let again = write_issue("retention", "Retention", "second")
                    .keep_named()
                    .await?;
                println!("{} {} {:?} {:?}", again.id == first.id, again.status, again.description, again.replace_key);
                Ok(())
            }
        "###;

        let (tmp, store) = open_store();
        let (_dir, compiled) = compile_source(SCANNER);
        let compiled = compiled.unwrap();
        let run = |n: u32, expected: &'static str| {
            let store = &store;
            let compiled = &compiled;
            let root = tmp.path().join(format!("staging{n}"));
            async move {
                let mut events = Vec::new();
                let config = ScanConfig {
                    staging_root: &root,
                    gage_version: "test-version",
                    dataset: None,
                    jobs: 1,
                };
                let outcome = scan(
                    store,
                    &config,
                    std::slice::from_ref(compiled),
                    &CancellationToken::new(),
                    |e| events.push(e),
                )
                .await
                .unwrap();
                assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
                assert_eq!(outputs(&events), [&Output::Println(expected.into())]);
                outcome
            }
        };

        let first = run(1, "true open Some(\"first\") Some(\"retention\")").await;
        let issues = IssueStore::from(&store);
        let record = ScanStore::from(&store).get(&first.id).unwrap();
        assert_eq!(record.content.issues.len(), 1);
        let issue = issues.at_commit(&record.content.issues[0]).unwrap();
        assert_eq!(issue.description.as_deref(), Some("first"));
        issues
            .set_status(&issue.id, IssueStatus::Closed, None, "user:t", None)
            .unwrap();

        let second = run(2, "true closed Some(\"first\") Some(\"retention\")").await;
        let record = ScanStore::from(&store).get(&second.id).unwrap();
        assert!(
            record.content.issues.is_empty(),
            "the second scan wrote no issue"
        );
        let live = issues.get(&issue.id).unwrap();
        assert_eq!(
            live.commit_sha,
            issues.at_commit(&live.commit_sha).unwrap().commit_sha
        );
        assert_eq!(live.status, IssueStatus::Closed, "the close stands");
        assert_eq!(issues.query().name("retention").count().unwrap(), 1);
    }

    /// Add a second `claude` session with `native_id` and `jsonl` to
    /// the seeded dataset. Returns the dataset's new commit and the
    /// session's Gage id.
    fn add_session(
        root: &std::path::Path,
        store: &Store,
        dataset_id: &str,
        native_id: &str,
        jsonl: &str,
    ) -> (String, String) {
        use gage_session::Driver;
        use gage_store::SessionSpec;

        let claude = root.join("claude");
        let dir = claude.join("projects").join("-home-alice-proj");
        std::fs::write(dir.join(format!("{native_id}.jsonl")), jsonl).unwrap();
        let driver = gage_claude::driver::ClaudeDriver::new();
        let source = driver
            .open_source(&format!("claude:{}", claude.display()))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let datasets = DatasetStore::from(store);
        let outcomes = datasets
            .sessions_add(
                dataset_id,
                vec![SessionSpec {
                    driver: &driver,
                    session: &mut *native,
                }],
            )
            .unwrap();
        (
            datasets.get(dataset_id).unwrap().commit_sha,
            outcomes[0].id.clone(),
        )
    }

    /// `newest_first()` reads the sessions newest-modified first, on
    /// its own and through `unseen`; the default is member order.
    #[tokio::test]
    async fn sessions_read_newest_first_on_request() {
        const SCANNER: &str = r#"
            use gage::scan;

            pub const SCANNER = #{
                name: "order",
                description: "Session order",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                for s in scan().sessions().await {
                    println!("member {}", s.id);
                }
                for s in scan().sessions().newest_first().await {
                    println!("newest {}", s.id);
                }
                for (s, _) in scan().sessions().newest_first().unseen("k").await? {
                    println!("unseen {}", s.id);
                }
                Ok(())
            }
        "#;
        const SESSION: &str = concat!(
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
        );

        let (tmp, store) = open_store();
        let (dataset_id, _, first) = seeded_dataset(tmp.path(), &store, SESSION);
        // `modified` is the store's write time in millis, so the
        // second add must land in a later millisecond
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (dataset_sha, second) = add_session(
            tmp.path(),
            &store,
            &dataset_id,
            "22222222-2222-3333-4444-555555555555",
            SESSION,
        );

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
        assert_eq!(
            outputs(&events),
            [
                &Output::Println(format!("member {first}")),
                &Output::Println(format!("member {second}")),
                &Output::Println(format!("newest {second}")),
                &Output::Println(format!("newest {first}")),
                &Output::Println(format!("unseen {second}")),
                &Output::Println(format!("unseen {first}")),
            ]
        );
    }

    /// Re-add the seeded session with `jsonl` as its grown content,
    /// advancing its slot in the dataset. Returns the dataset's new
    /// commit.
    fn grow_dataset(
        root: &std::path::Path,
        store: &Store,
        dataset_id: &str,
        jsonl: &str,
    ) -> String {
        use gage_session::Driver;
        use gage_store::SessionSpec;

        let claude = root.join("claude");
        let native_id = "11111111-2222-3333-4444-555555555555";
        let dir = claude.join("projects").join("-home-alice-proj");
        std::fs::write(dir.join(format!("{native_id}.jsonl")), jsonl).unwrap();
        let driver = gage_claude::driver::ClaudeDriver::new();
        let source = driver
            .open_source(&format!("claude:{}", claude.display()))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let datasets = DatasetStore::from(store);
        let outcomes = datasets
            .sessions_add(
                dataset_id,
                vec![SessionSpec {
                    driver: &driver,
                    session: &mut *native,
                }],
            )
            .unwrap();
        assert_eq!(outcomes[0].outcome, gage_store::SessionOutcome::Updated);
        datasets.get(dataset_id).unwrap().commit_sha
    }

    const WATERMARK_SCANNER: &str = r#"
        use gage::{carry_forward_notes, scan, watermark, write_note};

        pub const SCANNER = #{
            name: "wm",
            description: "Watermarks",
            tasks: #{ main: #{} },
        };

        const KEY = ("wm", "main", 1);

        pub async fn main() {
            let carried = carry_forward_notes(KEY).await?;
            println!("carried {carried}");
            for (s, unseen) in scan().sessions().unseen(KEY).await? {
                println!("unseen {unseen:?}");
                if let Some((start, end)) = unseen {
                    write_note("seen", format!("{start}-{end}"))
                        .for_session_range(s.id, start, end)
                        .work_key(KEY)
                        .await?;
                    write_note("untagged", "x").for_session(s.id).await?;
                    watermark(s, KEY).await?;
                }
            }
            match watermark("not-a-member", KEY).await {
                Err(gage::Error::Args(m)) => println!("args: {m}"),
                other => println!("unexpected: {other:?}"),
            }
            match carry_forward_notes("a/b").await {
                Err(gage::Error::Args(m)) => println!("args: {m}"),
                other => println!("unexpected: {other:?}"),
            }
            Ok(())
        }
    "#;

    const ONE_LINE: &str = concat!(
        r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
        "\n",
    );

    const THREE_LINES: &str = concat!(
        r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a1","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"hi"}]}}"#,
        "\n",
        r#"{"type":"user","uuid":"u2","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user","content":"more"}}"#,
        "\n",
    );

    /// Run the watermark scanner on `dataset_sha` and return the
    /// outcome with the task's printed lines.
    async fn run_watermark_scan(
        tmp: &TempDir,
        store: &Store,
        compiled: &CompiledScanner,
        dataset_sha: &str,
    ) -> (ScanOutcome, Vec<String>) {
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(dataset_sha),
            jobs: 1,
        };
        let outcome = scan(
            store,
            &config,
            std::slice::from_ref(compiled),
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
        let printed = outputs(&events)
            .into_iter()
            .map(|o| match o {
                Output::Println(s) => s.clone(),
                other => panic!("unexpected output {other:?}"),
            })
            .collect();
        (outcome, printed)
    }

    /// The first scan finds the whole session unseen and watermarks
    /// it; a second scan of the same dataset commit finds nothing
    /// unseen and carries the tagged note; a scan of the grown
    /// session finds the appended lines unseen and still carries the
    /// note written against the earlier commit; a scan of the
    /// earlier dataset commit again ignores the later scan's
    /// watermark and note, which sit on a descendant commit.
    #[tokio::test]
    async fn watermarks_resume_grown_sessions_and_carry_tagged_notes() {
        let (tmp, store) = open_store();
        let (dataset_id, dataset_sha_1, session_id) = seeded_dataset(tmp.path(), &store, ONE_LINE);
        let (_dir, compiled) = compile_source(WATERMARK_SCANNER);
        let compiled = compiled.unwrap();
        let args_lines = [
            "args: session not-a-member is not a member of the scan".to_string(),
            "args: key must be non-empty and must not contain '/': \"a/b\"".to_string(),
        ];

        let (first, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 0".to_string(),
                "unseen Some((1, 1))".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
            ]
        );
        let scans = ScanStore::from(&store);
        let first_record = scans.get(&first.id).unwrap();
        assert_eq!(first_record.content.notes.len(), 2);
        assert!(first_record.content.notes_carried.is_empty());
        let member_sha_1 = DatasetStore::from(&store)
            .sessions_at(&dataset_sha_1)
            .unwrap()[0]
            .commit_sha
            .clone();
        assert_eq!(
            first_record.content.watermarks,
            [gage_store::Watermark {
                kind: "sessions".into(),
                oid: session_id.clone(),
                key: "wm:main:1".into(),
                commit: member_sha_1.clone(),
            }]
        );
        let notes = NoteStore::from(&store);
        let tagged_1 = first_record
            .content
            .notes
            .iter()
            .map(|sha| (sha.clone(), notes.at_commit(sha).unwrap()))
            .find(|(_, n)| n.name == "seen")
            .expect("the scanner wrote the tagged note");
        assert_eq!(tagged_1.1.work_key.as_deref(), Some("wm:main:1"));
        assert_eq!(
            tagged_1.1.target.as_deref(),
            Some(format!("session:{session_id}#1-1").as_str())
        );

        let (second, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "unseen None".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
            ]
        );
        let second_record = scans.get(&second.id).unwrap();
        assert!(second_record.content.notes.is_empty());
        assert!(second_record.content.watermarks.is_empty());
        assert_eq!(
            second_record.content.notes_carried,
            [tagged_1.0.clone()],
            "only the tagged note is carried"
        );
        assert!(
            store
                .read_commit(&second.commit_sha)
                .unwrap()
                .parents
                .contains(&tagged_1.0),
            "a carried note is a commit parent"
        );

        let dataset_sha_2 = grow_dataset(tmp.path(), &store, &dataset_id, THREE_LINES);
        assert_ne!(dataset_sha_2, dataset_sha_1);
        let (third, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_2).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "unseen Some((2, 3))".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
            ]
        );
        let third_record = scans.get(&third.id).unwrap();
        assert_eq!(third_record.content.notes.len(), 2);
        assert_eq!(third_record.content.notes_carried, [tagged_1.0.clone()]);
        let member_sha_2 = DatasetStore::from(&store)
            .sessions_at(&dataset_sha_2)
            .unwrap()[0]
            .commit_sha
            .clone();
        assert_ne!(member_sha_2, member_sha_1);
        assert_eq!(third_record.content.watermarks[0].commit, member_sha_2);

        let (fourth, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "unseen None".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
            ]
        );
        let fourth_record = scans.get(&fourth.id).unwrap();
        assert_eq!(
            fourth_record.content.notes_carried,
            [tagged_1.0.clone()],
            "the third scan's note targets a descendant commit and is not carried"
        );

        let ctx = gage_query2::ContextBuilder::new(Some(Arc::new(Mutex::new(
            Store::open(store.path()).unwrap(),
        ))))
        .build()
        .await;
        let batches = ctx
            .sql("SELECT scan_id, commit FROM scan_watermark WHERE kind = 'sessions'")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(rows, 2, "the first and third scans hold watermarks");
        let carried = ctx
            .sql(&format!(
                "SELECT note_id FROM scan_note WHERE scan_id = '{}' AND carried",
                third.id
            ))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(carried.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
    }

    /// Tasks with `wants`, one of them unmatched, behind one failing
    /// writer. Task order is by name: chained, wanty, write.
    const DEPENDENT: &str = r#"
        pub const SCANNER = #{
            name: "deps",
            description: "Dependent tasks",
            tasks: #{
                write: #{ notes: #{ writes: #{ "x": "the x note" } } },
                wanty: #{ notes: #{ wants: ["x", "nobody"], writes: #{ "y": "the y note" } } },
                chained: #{ notes: #{ wants: ["y"] } },
            },
        };

        pub fn write() {
            Err("boom")
        }

        pub fn wanty() {
            println!("wanty ran");
        }

        pub fn chained() {
            println!("chained ran");
        }
    "#;

    #[tokio::test]
    async fn a_failed_writer_does_not_hold_back_the_tasks_ordered_after_it() {
        let (_dir, compiled) = compile_source(DEPENDENT);
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(
            outcome.attrs.tasks,
            TaskCounts {
                total: 3,
                completed: 2,
                failed: 1,
                skipped: 0,
            }
        );
        assert_eq!(
            events[0],
            Event::Warning {
                scanner: "deps".into(),
                task: "wanty".into(),
                message: "wants note 'nobody' but no task writes it".into(),
            },
            "the plan warning comes before any task"
        );
        let finished: Vec<(&str, TaskStatus)> = events
            .iter()
            .filter_map(|e| match e {
                Event::TaskFinished { task, status, .. } => Some((task.as_str(), *status)),
                _ => None,
            })
            .collect();
        assert_eq!(
            finished,
            [
                ("write", TaskStatus::Failed),
                ("wanty", TaskStatus::Completed),
                ("chained", TaskStatus::Completed),
            ],
            "release is in order: the failure releases wanty, and wanty releases chained"
        );
        assert!(events.contains(&Event::Output(TaskOutput {
            scanner: "deps".into(),
            task: "wanty".into(),
            output: Output::Println("wanty ran".into()),
        })));
        assert!(events.contains(&Event::Output(TaskOutput {
            scanner: "deps".into(),
            task: "chained".into(),
            output: Output::Println("chained ran".into()),
        })));
        assert!(
            events.last().unwrap()
                == &Event::Scan(ScanOutput::Out(format!(
                    "Scan {} completed: 3 tasks: 2 completed, 1 failed\n",
                    short_uuid(&outcome.id)
                )))
        );

        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        let task = |name: &str| {
            record
                .content
                .tasks
                .iter()
                .find(|t| t.task == name)
                .unwrap()
                .attrs
                .clone()
        };
        assert!(task("wanty").started.is_some() && task("wanty").stopped.is_some());
        let scans = ScanStore::from(&store);
        let records = String::from_utf8(
            scans
                .scan_log(&outcome.commit_sha, "records")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            records.contains(" WARN deps:wanty: wants note 'nobody' but no task writes it\n"),
            "{records}"
        );
        let plan: serde_json::Value =
            serde_json::from_slice(&scans.plan_file(&outcome.commit_sha).unwrap().unwrap())
                .unwrap();
        assert_eq!(
            plan["tasks"][1],
            serde_json::json!({
                "task": "deps:wanty",
                "selected": "explicit",
                "after": [{ "task": "deps:write", "pattern": "x" }],
                "unmatched": ["nobody"]
            })
        );
        assert_eq!(
            plan["tasks"][0]["after"],
            serde_json::json!([{ "task": "deps:wanty", "pattern": "y" }])
        );
    }

    const PARALLEL: &str = r#"
        pub const SCANNER = #{
            name: "par",
            description: "Independent tasks and one downstream task",
            tasks: #{
                a: #{ notes: #{ writes: #{ "n": "the n note" } } },
                b: #{},
                c: #{ notes: #{ wants: ["n"] } },
            },
        };

        pub fn a() {}
        pub fn b() {}
        pub fn c() {}
    "#;

    #[tokio::test]
    async fn the_pool_starts_ready_tasks_together_and_holds_downstream_tasks() {
        let (_dir, compiled) = compile_source(PARALLEL);
        let (tmp, store) = open_store();
        let mut events = Vec::new();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: None,
            jobs: 2,
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.completed, 3);
        let started = |task: &str| {
            events
                .iter()
                .position(|e| matches!(e, Event::TaskStarted { task: t, .. } if t == task))
                .unwrap()
        };
        let finished = |task: &str| {
            events
                .iter()
                .position(|e| matches!(e, Event::TaskFinished { task: t, .. } if t == task))
                .unwrap()
        };
        assert_eq!(
            (started("a"), started("b")),
            (0, 1),
            "both ready tasks start before either finishes: {events:?}"
        );
        assert!(
            started("c") > finished("a"),
            "c waits for a, which writes what it wants: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_pulled_in_scanner_plans_only_its_pulled_tasks() {
        let (_w, writer) = compile_source(
            r#"
            pub const SCANNER = #{
                name: "main",
                description: "Writes x",
                tasks: #{ w: #{ notes: #{ writes: #{ "x": "the x note" } } } },
            };

            pub fn w() {}
            "#,
        );
        let lib_dir = tempfile::tempdir().unwrap();
        let lib_path = lib_dir.path().join("scanner.rn");
        std::fs::write(
            &lib_path,
            r#"
            pub const SCANNER = #{
                name: "lib",
                description: "Pulled in by x",
                library: true,
                tasks: #{
                    a: #{},
                    b: #{ notes: #{ required_by: ["x"] } },
                },
            };

            pub fn a() {
                println!("a must not run");
            }

            pub fn b() {}
            "#,
        )
        .unwrap();
        let lib_def = parse_scanner_file(&lib_path).unwrap();
        let lib = compile(&Scanner::with_tasks(&lib_def, vec!["b".to_string()])).unwrap();
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("staging"),
            &[writer.unwrap(), lib],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(outcome.attrs.tasks.total, 2);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::TaskStarted { task, .. } if task == "a"))
        );
        let scans = ScanStore::from(&store);
        let plan: serde_json::Value =
            serde_json::from_slice(&scans.plan_file(&outcome.commit_sha).unwrap().unwrap())
                .unwrap();
        assert_eq!(
            plan["tasks"][0],
            serde_json::json!({
                "task": "lib:b",
                "selected": "required_by:x",
                "after": [{ "task": "main:w", "pattern": "x" }],
                "unmatched": []
            })
        );
        assert_eq!(plan["tasks"][1]["selected"], "explicit");
    }

    /// The `examples/scanners2/note_deps.rn` shape: `a` writes a dated
    /// note per session, `b` wants `a` and counts them.
    #[tokio::test]
    async fn a_downstream_task_reads_the_notes_its_upstream_task_wrote() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::{DateTime, scan, write_note};

            pub const SCANNER = #{
                name: "note-deps",
                description: "Note written for another note",
                tasks: #{
                    a: #{ notes: #{ writes: ["a"] } },
                    b: #{ notes: #{ writes: ["b"], wants: ["a"] } },
                },
            };

            pub async fn a() {
                let now = DateTime::from_millis(1_700_000_000_000);
                for s in scan().sessions().await {
                    write_note("a", now).for_session(s.id).await?;
                }
            }

            pub async fn b() {
                let notes = scan().notes().name("a").await?;
                let none = scan().notes().names(["nobody"]).await?;
                write_note("b", notes.len())
                    .metadata(#{ at: DateTime::from_millis(0), none: none.len(), first: notes[0].name })
                    .await?;
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (_, dataset_sha, session_id) = seeded_dataset(
            tmp.path(),
            &store,
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
        );
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 2,
        };
        let mut events = Vec::new();
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.attrs.tasks,
            TaskCounts {
                total: 2,
                completed: 2,
                failed: 0,
                skipped: 0,
            },
            "{events:?}"
        );
        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        let notes = gage_store::NoteStore::from(&store);
        let mut written: Vec<gage_store::NoteFull> = record
            .content
            .notes
            .iter()
            .map(|sha| notes.at_commit(sha).unwrap())
            .collect();
        written.sort_by(|x, y| x.name.cmp(&y.name));
        let [a, b] = written.as_slice() else {
            panic!("two notes: {written:?}");
        };
        assert_eq!(a.name, "a");
        assert_eq!(
            a.value,
            gage_store::NoteValue::Text("2023-11-14T22:13:20+00:00".into()),
            "a DateTime value is stored as its RFC 3339 string"
        );
        assert_eq!(
            a.target.as_deref(),
            Some(format!("session:{session_id}").as_str())
        );
        assert_eq!(b.name, "b");
        assert_eq!(b.value, gage_store::NoteValue::Json(serde_json::json!(1)));
        assert_eq!(
            b.metadata,
            Some(serde_json::json!({
                "at": "1970-01-01T00:00:00+00:00",
                "none": 0,
                "first": "a"
            })),
            "a DateTime inside metadata is stored as its RFC 3339 string"
        );
    }
}
