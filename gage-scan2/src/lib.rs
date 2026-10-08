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
//! tasks finish, and records the run in [`scan_dir`] and then the
//! store. The runtime is a pure event emitter: [`scan`] hands each
//! [`Event`] to the caller's sink, which owns rendering.
//!
//! Rule: code in this crate and in `gage-runtime2` never calls
//! `println!` or `eprintln!`. Every line meant for a person is emitted
//! as [`Event::Scan`] output, which the scan writes to its `logs/out`
//! or `logs/err` before the sink shows it, so the stored record and
//! the terminal hold the same text. Runtime diagnostics go through
//! `tracing` and reach the record through [`trace`].

pub mod attach;
pub mod plan;
pub mod scan_dir;
pub mod trace;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gage_core::datetime::now_ms;
use gage_core::uuid::{new_uuid, short_uuid};
use gage_registry::scanner::{Scanner, TaskDef, TaskKind};
use gage_runtime2::source::{SourceError, SourceFile, source_files};
use gage_runtime2::{
    CURRENT_RUNTIME_SCHEME, Fail, Level, OUTPUT_SINK, Output, OutputSink, SCAN_CTX, ScanContext,
    ScanDatasetRef, TaskOutput, is_ignore,
};
use gage_scan::error::render_task_error;
use gage_session::Driver;
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
use tracing::Instrument;

use crate::plan::{Plan, PlanError, PlannedScanner, Selection};
use crate::scan_dir::{Logs, ScanDir, ScannerPlan, State};
use crate::trace::{LOG_SCOPE, LogScope};

/// One item of run output, in the order it happened.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The plan is written and the scan directory exists, before any
    /// task runs. `tasks` are `(scanner, task)` in dispatch order.
    Started {
        id: String,
        tasks: Vec<(String, String)>,
    },
    /// Task output, already recorded in the scan's `logs/`
    Output(TaskOutput),
    /// The scan's own output for a person, already recorded in the
    /// scan's `logs/`
    Scan(ScanOutput),
    /// The scan's closing line, given once after every task has
    /// reached a terminal status. Already recorded in the scan's
    /// `logs/out` as [`summary_line`] over the short id; the sink
    /// renders the id its own way.
    Summary {
        id: String,
        attrs: ScanAttrs,
    },
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
    /// The `SCANNER` declaration has defects; `diagnostics` labels each
    Invalid {
        name: String,
        diagnostics: String,
    },
    Compile {
        name: String,
        diagnostics: String,
    },
    /// A declared task names a function the scanner does not define
    MissingTask {
        scanner: String,
        task: String,
        function: String,
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
            Error::Invalid { name, diagnostics } => {
                write!(
                    f,
                    "scanner {name} has an invalid SCANNER declaration\n{diagnostics}"
                )
            }
            Error::Compile { name, diagnostics } => {
                write!(f, "scanner {name} failed to compile\n{diagnostics}")
            }
            Error::MissingTask {
                scanner,
                task,
                function,
            } => {
                write!(
                    f,
                    "scanner {scanner} declares task {task} but defines no function {function}"
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
    /// The directory holding the scanner's source file; an attach
    /// task's default file root
    dir: PathBuf,
    /// The declared tasks by name, scan and attach alike. A scan plans
    /// the scan tasks; the attach phase runs the attach tasks
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

    /// The directory holding the scanner's source file.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The compiled artifacts a worker needs to run one of the
    /// scanner's tasks.
    fn task_unit(&self) -> TaskUnit {
        TaskUnit {
            rt: self.rt.clone(),
            unit: self.unit.clone(),
            sources: Arc::clone(&self.sources),
            params: self.params.clone(),
            functions: self
                .tasks
                .iter()
                .map(|(name, def)| (name.clone(), def.call.clone()))
                .collect(),
        }
    }
}

/// Compile a scanner and verify that every declared task and
/// attachment maps to a function the scanner defines. A scanner that
/// fails here is a full stop for the caller: nothing has run yet.
pub fn compile(scanner: &Scanner<'_>) -> Result<CompiledScanner, Error> {
    let def = scanner.def;
    if !def.problems.is_empty() {
        return Err(Error::Invalid {
            name: def.name.clone(),
            diagnostics: def.render_problems(),
        });
    }
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
    for task in def.tasks.values() {
        if vm.lookup_function([task.call.as_str()]).is_err() {
            return Err(Error::MissingTask {
                scanner: def.name.clone(),
                task: task.name.clone(),
                function: task.call.clone(),
            });
        }
    }

    Ok(CompiledScanner {
        name: def.name.clone(),
        dir: def.path.parent().map(Path::to_path_buf).unwrap_or_default(),
        tasks: def.tasks.clone(),
        selection: Selection::Explicit,
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
    /// The scan directory could not be written
    ScanDir(io::Error),
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
            ScanError::ScanDir(e) => write!(f, "writing scan directory: {e}"),
            ScanError::Store(e) => write!(f, "writing scan to the store: {e}"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<io::Error> for ScanError {
    fn from(e: io::Error) -> Self {
        ScanError::ScanDir(e)
    }
}

impl From<StoreError> for ScanError {
    fn from(e: StoreError) -> Self {
        ScanError::Store(e)
    }
}

/// Where a scan's directory lives, how many tasks it runs at once, and what it
/// records about its runtime.
pub struct ScanConfig<'a> {
    /// The parent of scan directories, `scans/` under Gage home in production
    pub scans_dir: &'a std::path::Path,
    /// The Gage build version; the scan records
    /// `<CURRENT_RUNTIME_SCHEME> <version>` as its `runtime`
    pub gage_version: &'a str,
    /// The commit SHA of the dataset to scan, linked from the scan as
    /// `dataset.link`. `None` runs the scanners with no dataset.
    pub dataset: Option<&'a str>,
    /// Tasks run at once. Treated as at least 1.
    pub jobs: usize,
    /// The driver that runs the scan's agents
    pub driver: Arc<dyn Driver>,
    /// Ignore prior work: every object's high-water mark is 0 and no
    /// notes are carried forward. Watermarks are still written.
    pub invalidate: bool,
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
/// The scan runs in its scan directory, `config.scans_dir/<id>/`,
/// (see [`scan_dir`]) and applied to the store at its terminal state,
/// after which the scan directory is removed. Output and task
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
        .map(|s| {
            s.tasks
                .values()
                .filter(|t| t.kind() == TaskKind::Scan)
                .map(|t| t.name.clone())
                .collect()
        })
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
    let scan_dir = ScanDir::create(config.scans_dir, &id, config.dataset, &scanner_plans)?;
    scan_dir.write_plan(&plan.to_json())?;
    let mut on_event = on_event;
    on_event(Event::Started {
        id: id.clone(),
        tasks: plan
            .tasks
            .iter()
            .map(|t| (t.scanner.clone(), t.task.clone()))
            .collect(),
    });
    let mut scan_ctx = ScanContext::new(
        id.clone(),
        dataset,
        store.path(),
        scan_dir.layout(),
        Arc::clone(&config.driver),
    )?;
    scan_ctx.invalidate = config.invalidate;
    trace::install_panic_hook();
    let scope = LogScope {
        object_dir: scan_dir.object_dir(),
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
        scan_dir,
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
    scan_dir: ScanDir,
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
        tracing::info!(
            id = %self.id,
            tasks = plan.tasks.len(),
            dataset = self.scan_ctx.dataset.as_ref().map(|d| d.id.as_str()),
            commit = self.scan_ctx.dataset.as_ref().map(|d| d.commit_sha.as_str()),
            jobs = self.config.jobs.max(1),
            "scan started"
        );
        let mut scan_logs = self.scan_dir.scan_logs();
        let started = now_ms();
        for t in &plan.tasks {
            for pattern in &t.unmatched_note_wants {
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
        scan_logs.out(&format!("{}\n", summary_line(short_uuid(&self.id), &attrs)))?;
        (self.on_event)(Event::Summary {
            id: self.id.clone(),
            attrs: attrs.clone(),
        });
        drop(scan_logs);
        if let Some(e) = self.scope.failure.lock().unwrap().take() {
            return Err(ScanError::ScanDir(e));
        }
        self.scan_dir.write_scan(&attrs)?;
        self.scan_dir.set_state(if canceled {
            State::Canceled
        } else {
            State::Completed
        })?;
        // Apply: the scan's notes become objects first, so the scan
        // can link them
        let notes = NoteStore::from(store);
        let mut note_shas = Vec::new();
        let mut note_commits: HashMap<String, String> = HashMap::new();
        for dir in self.scan_dir.note_dirs()? {
            let (id, sha) = notes.create_from_dir(&dir)?;
            note_shas.push(sha.clone());
            note_commits.insert(id, sha);
        }
        self.scan_dir.write_notes_link(&note_shas)?;
        // A watermark on one of the scan's notes waited for its commit
        for (id, key, mark) in self.scan_dir.note_watermarks()? {
            let Some(commit) = note_commits.get(&id) else {
                return Err(ScanError::ScanDir(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("note watermark names {id}, which the scan did not write"),
                )));
            };
            self.scan_dir.write_watermark(&id, &key, commit, mark)?;
        }
        let carried = self.scan_dir.carried_notes()?;
        self.scan_dir.write_notes_carried_link(&carried)?;
        // Issues follow the notes they cite, so their evidence resolves
        let issues = IssueStore::from(store);
        let mut issue_shas = Vec::new();
        for dir in self.scan_dir.issue_dirs()? {
            let (_, sha) = issues.apply_from_dir(&dir)?;
            issue_shas.push(sha);
        }
        self.scan_dir.write_issues_link(&issue_shas)?;
        let commit_sha = ScanStore::from(store).create(&self.id, &self.scan_dir.object_dir())?;
        tracing::info!(
            notes = note_shas.len(),
            issues = issue_shas.len(),
            carried = carried.len(),
            commit = %commit_sha,
            "scan applied"
        );
        self.scan_dir.mark_applied()?;
        self.scan_dir.remove()?;
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
            let unstarted = d.status.iter().filter(|s| s.is_none()).count() - d.running.len();
            tracing::info!(aborted = d.running.len(), unstarted, "scan canceled");
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
        self.scan_dir.write_task(
            &t.scanner,
            &t.task,
            &task_attrs(TaskStatus::Started, Some(now), None),
        )?;
        (self.on_event)(Event::TaskStarted {
            scanner: t.scanner.clone(),
            task: t.task.clone(),
        });
        tracing::info!(scanner = %t.scanner, task = %t.task, "task started");
        let unit = self.units[&t.scanner].clone();
        let mut ctx = self.scan_ctx.clone();
        ctx.params = unit.params.clone();
        ctx.scanner = t.scanner.clone();
        ctx.task = t.task.clone();
        ctx.sources = Some(Arc::clone(&unit.sources));
        let function = unit.functions[&t.task].clone();
        let exec = TaskExec {
            unit,
            task: t.task.clone(),
            function,
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
        self.scan_dir.write_task(&t.scanner, &t.task, &attrs)?;
        tracing::info!(
            scanner = %t.scanner,
            task = %t.task,
            status = status.as_str(),
            elapsed_ms = d.started[i].zip(stopped).map(|(a, b)| b - a),
            "task finished"
        );
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

/// The scan's closing line: `Scan <shown id> completed: 3 tasks: 2
/// completed, 1 failed`, with skipped and canceled counts when
/// nonzero. `shown_id` is the id as the caller displays it: the plain
/// short id in the scan log, a styled one on a terminal.
pub fn summary_line(shown_id: &str, attrs: &ScanAttrs) -> String {
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
        "Scan {shown_id} {state}: {} tasks: {}",
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
    /// The function each task runs, by task name
    functions: BTreeMap<String, String>,
}

/// Everything a worker needs to run one task: the scanner's
/// artifacts and the task-local scopes the task runs under.
struct TaskExec {
    unit: TaskUnit,
    task: String,
    /// The function that runs the task
    function: String,
    ctx: ScanContext,
    sink: OutputSink,
    scope: LogScope,
}

impl TaskExec {
    /// Run the task under its scan context, output sink, log scope,
    /// and a `task` span naming the scanner and task. The span is
    /// created inside the log scope so the records layer can attach
    /// the scope to it.
    async fn run(self) -> Result<(), String> {
        let TaskExec {
            unit,
            task,
            function,
            ctx,
            sink,
            scope,
        } = self;
        LOG_SCOPE
            .scope(scope, async move {
                let span = tracing::info_span!("task", scanner = %ctx.scanner, task = %task);
                SCAN_CTX
                    .scope(ctx, OUTPUT_SINK.scope(sink, execute(&unit, &function)))
                    .instrument(span)
                    .await
            })
            .await
    }
}

/// Run `function` on a fresh VM and interpret its return value.
pub(crate) async fn execute(scanner: &TaskUnit, function: &str) -> Result<(), String> {
    let vm = Vm::new(scanner.rt.clone(), scanner.unit.clone());
    let execution = vm
        .send_execute([function], ())
        .map_err(|e| vm_error(&e, &scanner.sources))?;
    let value = execution
        .complete()
        .await
        .map_err(|e| vm_error(&e, &scanner.sources))?;
    task_result(value, scanner, function)
}

/// Render a VM error as Rune does: the diagnostic with its source
/// excerpt, then a `Backtrace:` section listing every frame.
fn vm_error(e: &VmError, sources: &Sources) -> String {
    gage_runtime2::render_vm_error(e, Some(sources))
}

/// Interpret a task's return value. A task returning unit, `Ok`, or
/// `Err(Ignore)` succeeded. `Err(Fail(msg))` is the scanner author's
/// message to the user: the task fails with `msg` alone. Any other
/// `Err` is a scanner defect: the task fails with a diagnostic naming
/// the value and pointing at the task function.
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the runtime holds the only live handle"
)]
fn task_result(value: Value, scanner: &TaskUnit, task: &str) -> Result<(), String> {
    match rune::from_value::<Result<Value, Value>>(value) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) if is_ignore(&err) => Ok(()),
        Ok(Err(err)) => {
            if let Ok(fail) = err.borrow_ref::<Fail>() {
                return Err(format!("error: {}\n", fail.message()));
            }
            Err(returned_error(&render_task_error(err), scanner, task))
        }
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
    use datafusion::arrow::array::StringArray;
    use gage_registry::scanner::{ScannerDef, parse_scanner_file};
    use gage_session::{
        AgentEvent, AgentMcp, AgentOutcome, AgentSession, AgentSpec, ContentSink, ContentSource,
        DriverError, NativeSession, SessionAttrs, Source, StoredSession,
    };
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

    /// The driver a test scan runs agents with when it runs none
    fn claude_driver() -> Arc<dyn Driver> {
        Arc::new(gage_claude::driver::ClaudeDriver::new())
    }

    /// A fresh store and scans directory under one directory.
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
            scans_dir: root,
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
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

    /// A driver whose agent replays a fixed event sequence and whose
    /// transcript is a fixed native session.
    struct ScriptedDriver {
        cleaned_up: Arc<std::sync::atomic::AtomicBool>,
        /// The MCP server the last `run_agent` was given
        mcp_seen: Arc<std::sync::Mutex<Option<AgentMcp>>>,
    }

    impl ScriptedDriver {
        fn new() -> Self {
            ScriptedDriver {
                cleaned_up: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                mcp_seen: Arc::new(std::sync::Mutex::new(None)),
            }
        }
    }

    struct ScriptedAgent {
        events: std::collections::VecDeque<AgentEvent>,
        /// The id of the native session the transcript reports
        native_id: String,
        project: Option<String>,
        cleaned_up: Arc<std::sync::atomic::AtomicBool>,
    }

    struct ScriptedNative {
        id: String,
        source: String,
        project: Option<String>,
    }

    impl SessionAttrs for ScriptedNative {
        fn native_mtime(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
        fn native_size(&self) -> u64 {
            0
        }
        fn is_empty(&self) -> bool {
            false
        }
        fn project_name(&self) -> Option<&str> {
            self.project.as_deref()
        }
        fn title(&self) -> Option<&str> {
            None
        }
        fn model(&self) -> Option<&str> {
            None
        }
        fn message_count(&self) -> Option<u64> {
            Some(2)
        }
        fn line_count(&self) -> Option<u64> {
            Some(2)
        }
    }

    impl NativeSession for ScriptedNative {
        fn id(&self) -> &str {
            &self.id
        }
        fn session_type(&self) -> &str {
            "scripted"
        }
        fn source(&self) -> &str {
            &self.source
        }
        fn attrs(&self) -> &dyn SessionAttrs {
            self
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[async_trait::async_trait]
    impl AgentSession for ScriptedAgent {
        async fn next_event(&mut self) -> Option<AgentEvent> {
            self.events.pop_front()
        }
        async fn send(&mut self, _text: &str) -> std::io::Result<()> {
            Ok(())
        }
        async fn interrupt(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn close_input(&mut self) {}
        async fn wait_exit(&mut self) -> std::io::Result<i32> {
            Ok(0)
        }
        async fn kill(&mut self, _grace: std::time::Duration) -> std::io::Result<()> {
            Ok(())
        }
        async fn take_stderr(&mut self) -> std::io::Result<Vec<u8>> {
            Ok(b"warned\n".to_vec())
        }
        fn transcript(&mut self) -> Result<Option<Box<dyn NativeSession + Send>>, DriverError> {
            Ok(Some(Box::new(ScriptedNative {
                id: self.native_id.clone(),
                source: format!("scripted:{}", self.native_id),
                project: self.project.clone(),
            })))
        }
        fn cleanup(&mut self) -> Result<(), DriverError> {
            self.cleaned_up
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    impl Driver for ScriptedDriver {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn version(&self) -> &'static str {
            "0.1"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["scripted"]
        }
        fn open_source(&self, _source: &str) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("open_source not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<String, DriverError> {
            use std::io::Write as _;
            let mut w = sink.create("session.jsonl").map_err(DriverError::Io)?;
            w.write_all(b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n")
                .map_err(DriverError::Io)?;
            Ok("scripted 1".to_string())
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("read_stored not used".into()))
        }
        fn run_agent(&self, spec: AgentSpec) -> Result<Box<dyn AgentSession>, DriverError> {
            assert_eq!(spec.prompt, "hello");
            assert_eq!(spec.model.as_deref(), Some("medium"));
            *self.mcp_seen.lock().unwrap() = spec.mcp.clone();
            let outcome = AgentOutcome {
                text: "hello there".into(),
                stop_reason: "end_turn".into(),
                turns: 1,
                session_id: "native-1".into(),
                raw: r#"{"type":"result","result":"hello there"}"#.into(),
                ..AgentOutcome::default()
            };
            Ok(Box::new(ScriptedAgent {
                events: [
                    AgentEvent::System(r#"{"subtype":"init"}"#.into()),
                    AgentEvent::Assistant("hello there".into()),
                    AgentEvent::TurnEnd {
                        outcome: Box::new(outcome),
                        idle: true,
                    },
                ]
                .into_iter()
                .collect(),
                native_id: "native-1".into(),
                project: spec.project,
                cleaned_up: Arc::clone(&self.cleaned_up),
            }))
        }
    }

    /// A driver whose agent answers with its prompt and whose
    /// transcript's native id is `native-<prompt>`, so several agents
    /// of one task store distinct sessions.
    struct EchoDriver {
        cleaned_up: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Driver for EchoDriver {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn version(&self) -> &'static str {
            "0.1"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["scripted"]
        }
        fn open_source(&self, _source: &str) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("open_source not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<String, DriverError> {
            use std::io::Write as _;
            let mut w = sink.create("session.jsonl").map_err(DriverError::Io)?;
            w.write_all(b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n")
                .map_err(DriverError::Io)?;
            Ok("scripted 1".to_string())
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("read_stored not used".into()))
        }
        fn run_agent(&self, spec: AgentSpec) -> Result<Box<dyn AgentSession>, DriverError> {
            let native_id = format!("native-{}", spec.prompt);
            let outcome = AgentOutcome {
                text: spec.prompt.clone(),
                stop_reason: "end_turn".into(),
                turns: 1,
                session_id: native_id.clone(),
                raw: String::new(),
                ..AgentOutcome::default()
            };
            Ok(Box::new(ScriptedAgent {
                events: [
                    AgentEvent::Assistant(spec.prompt),
                    AgentEvent::TurnEnd {
                        outcome: Box::new(outcome),
                        idle: true,
                    },
                ]
                .into_iter()
                .collect(),
                native_id,
                project: spec.project,
                cleaned_up: Arc::clone(&self.cleaned_up),
            }))
        }
    }

    /// A call that declares tools is handed to the driver with the
    /// tool service's URL and the tool names; a call without tools
    /// has no MCP server.
    #[tokio::test]
    async fn declared_tools_reach_the_driver_as_an_mcp_server() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::{Input, Tool, call_agent, tools::Query};

            pub const SCANNER = #{
                name: "agentic",
                description: "Runs an agent with a tool",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let secret = "abc";
                let agent = call_agent("hello")
                    .model("medium")
                    .tool(Tool::new("secret", |inputs| Ok(secret)).input(Input::string("key")))
                    .tools([Tool::new("ping", |inputs| Ok("pong")), Query::new()])
                    .await?;
                println!("{}", agent.wait().await?.text);
            }
            "#,
        );
        let driver = Arc::new(ScriptedDriver::new());
        let (tmp, store) = open_store();
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver: driver.clone(),
            invalidate: false,
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
        assert_eq!(outputs(&events), [&Output::Println("hello there".into())]);
        let mcp = driver.mcp_seen.lock().unwrap().clone().unwrap();
        assert!(mcp.url.starts_with("http://127.0.0.1:"), "{}", mcp.url);
        assert!(mcp.url.ends_with("/mcp"), "{}", mcp.url);
        assert_eq!(mcp.tool_names, ["secret", "ping", "Query"]);
    }

    /// A tool declaration the runtime rejects fails the task at
    /// `call_agent`, before any agent starts.
    #[tokio::test]
    async fn an_invalid_tool_declaration_fails_the_call() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::{Tool, call_agent};

            pub const SCANNER = #{
                name: "agentic",
                description: "Runs an agent with a bad tool",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                match call_agent("hello").model("medium").tool(Tool::new("no spaces", |i| Ok(1))).await {
                    Err(e) => println!("{e:?}"),
                    Ok(_) => println!("started"),
                }
            }
            "#,
        );
        let driver = Arc::new(ScriptedDriver::new());
        let (tmp, store) = open_store();
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver: driver.clone(),
            invalidate: false,
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
            [&Output::Println(
                r#"Agent(General("tool name \"no spaces\" is not 1 to 128 ASCII letters, digits, '_', or '-'"))"#
                    .into()
            )]
        );
        assert!(driver.mcp_seen.lock().unwrap().is_none());
    }

    /// `poll` delivers `Stop` once; a later `poll`, `send`, or `kill`
    /// fails with `AgentError::Stopped`, and `wait` returns the result
    /// again.
    #[tokio::test]
    async fn agent_calls_after_stop_fail_with_stopped() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::{AgentError, Error, Event, call_agent};

            pub const SCANNER = #{
                name: "agentic",
                description: "Runs an agent",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let agent = call_agent("hello").name("greeter").model("medium").await?;
                loop {
                    if let Event::Stop(r) = agent.poll().await? {
                        println!("stop: {r} {}", agent.running());
                        break;
                    }
                }
                match agent.poll().await {
                    Err(Error::Agent(AgentError::Stopped)) => println!("poll: stopped"),
                    other => println!("poll: {other:?}"),
                }
                match agent.send("more").await {
                    Err(Error::Agent(AgentError::Stopped)) => println!("send: stopped"),
                    other => println!("send: {other:?}"),
                }
                match agent.kill(1).await {
                    Err(Error::Agent(AgentError::Stopped)) => println!("kill: stopped"),
                    other => println!("kill: {other:?}"),
                }
                println!("wait: {}", agent.wait().await?.text);
            }
            "#,
        );
        let (tmp, store) = open_store();
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver: Arc::new(ScriptedDriver::new()),
            invalidate: false,
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
                &Output::Println("stop: end_turn false".into()),
                &Output::Println("poll: stopped".into()),
                &Output::Println("send: stopped".into()),
                &Output::Println("kill: stopped".into()),
                &Output::Println("wait: hello there".into()),
            ]
        );
    }

    /// `call_agent` runs the agent through the scan's driver, `poll`
    /// and `wait` expose its events and result, the transcript is
    /// stored as a session under the task's `session+task:` source,
    /// and the task's agent record is in the scan object.
    #[tokio::test]
    async fn tasks_run_agents_and_the_scan_records_them() {
        use gage_store::{AgentAttrs, SessionStore, TaskAgent, session_object_id};

        let (_dir, compiled) = compile_source(
            r#"
            use gage::{Event, call_agent};

            pub const SCANNER = #{
                name: "agentic",
                description: "Runs an agent",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let agent = call_agent("hello").name("greeter").model("medium").await?;
                while agent.running() {
                    match agent.poll().await? {
                        Event::Assistant(t) => println!("assistant: {t}"),
                        Event::TurnEnd(r) => println!("turn end: {r}"),
                        other => println!("other: {other:?}"),
                    }
                }
                let r = agent.wait().await?;
                println!("{} {} {} {}", r.text, r.stop_reason, r.exit_code, r.stderr.trim());
                println!("{:?}", r.as_metadata().turns);
            }
            "#,
        );
        let driver = Arc::new(ScriptedDriver::new());
        let cleaned_up = Arc::clone(&driver.cleaned_up);
        let (tmp, store) = open_store();
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver,
            invalidate: false,
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
                &Output::Println(r#"other: System({"subtype":"init"})"#.into()),
                &Output::Println("assistant: hello there".into()),
                &Output::Println("turn end: end_turn".into()),
                &Output::Println("hello there end_turn 0 warned".into()),
                &Output::Println("1".into()),
            ]
        );
        assert!(cleaned_up.load(std::sync::atomic::Ordering::SeqCst));

        let session_id = session_object_id("scripted", "native-1");
        let session = SessionStore::from(&store).get(&session_id).unwrap();
        assert_eq!(
            session.attrs.native_source,
            format!("session+task:{}/agentic:main/native-1", outcome.id)
        );
        assert_eq!(session.attrs.project.as_deref(), Some("greeter"));

        let scans = ScanStore::from(&store);
        let record = scans.get(&outcome.id).unwrap();
        let task = &record.content.tasks[0];
        assert_eq!(
            (task.scanner.as_str(), task.task.as_str()),
            ("agentic", "main")
        );
        assert_eq!(task.agent_sessions, [session.commit_sha.clone()]);
        assert_eq!(
            task.agents,
            [TaskAgent {
                id: session_id.clone(),
                attrs: AgentAttrs { exit_code: 0 },
            }]
        );
        assert_eq!(
            scans
                .agent_file(
                    &outcome.commit_sha,
                    "agentic",
                    "main",
                    &session_id,
                    "result"
                )
                .unwrap(),
            Some(br#"{"type":"result","result":"hello there"}"#.to_vec())
        );
        assert_eq!(
            scans
                .agent_file(
                    &outcome.commit_sha,
                    "agentic",
                    "main",
                    &session_id,
                    "stderr"
                )
                .unwrap(),
            Some(b"warned\n".to_vec())
        );
        assert!(
            store
                .read_commit(&outcome.commit_sha)
                .unwrap()
                .parents
                .contains(&session.commit_sha),
            "the agent session is a parent of the scan commit"
        );
    }

    /// `AgentRunner` runs every queued call and `next` yields each
    /// result with the context value it was queued with. The context
    /// value stays readable by the caller after `add`. Every run's
    /// transcript is stored and recorded on the task.
    #[tokio::test]
    async fn agent_runner_yields_each_result_with_its_context() {
        use gage_store::{SessionStore, session_object_id};

        let (_dir, compiled) = compile_source(
            r#"
            use gage::{AgentRunner, call_agent};

            pub const SCANNER = #{
                name: "runner",
                description: "Runs agents through a runner",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let runner = AgentRunner::new();
                let ctx = #{ session: "c" };
                runner.add(call_agent("a").name("agent-a"), #{ session: "a" });
                runner.add(call_agent("b").name("agent-b"), #{ session: "b" });
                runner.add(call_agent("c").name("agent-c"), ctx);
                println!("ctx {}", ctx.session);
                let results = runner.start();
                let seen = #{};
                while let Some(item) = results.next().await {
                    let (result, #{ session }) = item?;
                    seen[session] = format!("{}:{}", result.text, result.stop_reason);
                }
                println!("{} {} {}", seen.a, seen.b, seen.c);
                println!("{:?}", results.next().await);
            }
            "#,
        );
        let driver = Arc::new(EchoDriver {
            cleaned_up: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let (tmp, store) = open_store();
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver,
            invalidate: false,
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
                &Output::Println("ctx c".into()),
                &Output::Println("a:end_turn b:end_turn c:end_turn".into()),
                &Output::Println("None".into()),
            ]
        );

        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        let task = &record.content.tasks[0];
        let mut agent_ids: Vec<_> = task.agents.iter().map(|a| a.id.clone()).collect();
        agent_ids.sort();
        let mut expected_ids =
            ["a", "b", "c"].map(|p| session_object_id("scripted", &format!("native-{p}")));
        expected_ids.sort();
        assert_eq!(agent_ids, expected_ids);
        assert_eq!(task.agent_sessions.len(), 3);
        for p in ["a", "b", "c"] {
            let session = SessionStore::from(&store)
                .get(&session_object_id("scripted", &format!("native-{p}")))
                .unwrap();
            assert_eq!(
                session.attrs.native_source,
                format!("session+task:{}/runner:main/native-{p}", outcome.id)
            );
            assert_eq!(
                session.attrs.project.as_deref(),
                Some(format!("agent-{p}").as_str())
            );
        }
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outputs(&events), [&Output::Println("go".into())]);
        assert_eq!(outcome.unwrap().attrs.tasks.completed, 1);
    }

    /// `Err(Fail(msg))` is the scanner author's message for the user:
    /// the task fails with that message and no source excerpt.
    #[tokio::test]
    async fn task_returning_fail_fails_with_the_message_alone() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::Fail;

            pub const SCANNER = #{
                name: "s",
                description: "Fails with a message",
                tasks: #{ go: #{} },
            };

            pub fn go() {
                Err(Fail("add the attachment"))
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("scans"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        let failure = events
            .iter()
            .find_map(|e| match e {
                Event::TaskFinished {
                    error: Some(message),
                    ..
                } => Some(message.clone()),
                _ => None,
            })
            .expect("the task should finish with an error");
        assert_eq!(failure, "error: add the attachment\n");
        assert_eq!(outcome.unwrap().attrs.tasks.failed, 1);
    }

    /// `Err(Ignore)` is an early exit with nothing to do, not a failure.
    #[tokio::test]
    async fn task_returning_ignore_completes() {
        let (_dir, compiled) = compile_source(
            r#"
            use gage::Ignore;

            pub const SCANNER = #{
                name: "s",
                description: "Exits early",
                tasks: #{ go: #{} },
            };

            pub fn go() {
                Err(Ignore)
            }
            "#,
        );
        let (tmp, store) = open_store();
        let (outcome, events) = run_all(
            &store,
            &tmp.path().join("scans"),
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::TaskFinished { error: Some(_), .. })),
            "{events:?}"
        );
        let tasks = outcome.unwrap().attrs.tasks;
        assert_eq!((tasks.completed, tasks.failed), (1, 0));
    }

    /// The scan record lands in the store with one task record per
    /// task, the failed task's message in `logs/err`, and the scan directory
    /// removed once applied.
    #[tokio::test]
    async fn scan_records_every_task_and_removes_the_scan_dir() {
        let (_dir, compiled) = compile_source(FAIL_THEN_RUN);
        let (tmp, store) = open_store();
        let root = tmp.path().join("scans");
        let (outcome, events) = run_all(
            &store,
            &root,
            &[compiled.unwrap()],
            &CancellationToken::new(),
        )
        .await;
        let outcome = outcome.unwrap();
        let failure = match &events[3] {
            Event::TaskFinished {
                error: Some(message),
                ..
            } => message.clone(),
            other => panic!("expected the failure of task a, got {other:?}"),
        };
        let notice = format!("task fail:a failed\n{failure}");
        let summary = summary_line(short_uuid(&outcome.id), &outcome.attrs);
        assert_eq!(
            events,
            [
                Event::Started {
                    id: outcome.id.clone(),
                    tasks: vec![("fail".into(), "a".into()), ("fail".into(), "b".into()),],
                },
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
                Event::Summary {
                    id: outcome.id.clone(),
                    attrs: outcome.attrs.clone(),
                },
            ]
        );
        assert_eq!(
            summary,
            format!(
                "Scan {} completed: 2 tasks: 1 completed, 1 failed",
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
            Some(format!("b ran\n{summary}\n").into_bytes()),
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
            "the scan directory is removed after apply"
        );
    }

    /// Print output lands in the scan's `logs/out` in delivery order,
    /// and log records in `logs/records` with the task as origin.
    #[tokio::test]
    async fn task_output_and_records_are_stored_under_the_scan_logs() {
        let scanner_events = install_subscriber();
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
            &tmp.path().join("scans"),
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
            format!(
                "ab\n{}\n",
                summary_line(short_uuid(&outcome.id), &outcome.attrs)
            )
        );
        let records = scans
            .scan_log(&outcome.commit_sha, "records")
            .unwrap()
            .unwrap();
        let records = String::from_utf8(records).unwrap();
        // Runtime records share the file; the `log` macros' tracing
        // events do not, since the sink's copy is the one written
        let lines: Vec<&str> = records
            .lines()
            .filter(|l| l.contains(" logs:loud: "))
            .collect();
        assert_eq!(lines.len(), 2, "{records}");
        assert!(lines[0].ends_with("Z INFO logs:loud: count 3"), "{records}");
        assert!(lines[1].ends_with("Z WARN logs:loud: careful"), "{records}");
        assert!(!records.contains(" scanner::"), "{records}");
        let scanner_events = scanner_events.lock().unwrap();
        assert!(
            scanner_events.contains(&"INFO scanner: count 3 scanner=logs task=loud".to_string())
                && scanner_events
                    .contains(&"WARN scanner: careful scanner=logs task=loud".to_string()),
            "{scanner_events:?}"
        );
    }

    /// Install, once per process, the records layer and a layer that
    /// collects every event the `log` macros raise as `LEVEL target:
    /// message`. Process-wide: a thread-scoped subscriber would miss
    /// callsites other tests hit first with no subscriber, whose
    /// cached interest stays disabled. The records layer drops events
    /// outside a scan scope, so other tests are unaffected.
    fn install_subscriber() -> &'static Mutex<Vec<String>> {
        use std::sync::Once;
        use tracing_subscriber::layer::SubscriberExt;

        static SCANNER_EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry()
                    .with(trace::layer())
                    .with(ScannerEvents),
            )
            .unwrap();
        });
        &SCANNER_EVENTS
    }

    struct ScannerEvents;

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ScannerEvents {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let meta = event.metadata();
            if meta.target() != gage_runtime2::LOG_TARGET {
                return;
            }
            let mut message = trace::MessageVisitor::default();
            event.record(&mut message);
            install_subscriber().lock().unwrap().push(format!(
                "{} {}: {}",
                meta.level(),
                meta.target(),
                message.text
            ));
        }
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
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
            &tmp.path().join("scans"),
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
            events.get(1),
            Some(&Event::Scan(ScanOutput::Err("scan canceled\n".into()))),
            "the cancel notice is given once, right after the start"
        );
        assert_eq!(
            events.last(),
            Some(&Event::Summary {
                id: outcome.id.clone(),
                attrs: outcome.attrs.clone(),
            })
        );
        assert_eq!(
            summary_line(short_uuid(&outcome.id), &outcome.attrs),
            format!(
                "Scan {} canceled: 2 tasks: 0 completed, 0 failed, 2 canceled",
                short_uuid(&outcome.id)
            )
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
        install_subscriber();

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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
            records
                .lines()
                .any(|l| l.contains(" INFO gage_scan2: scan started id=")
                    && l.ends_with(" tasks=1 jobs=1")),
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
    async fn duplicate_scanner_names_are_rejected_before_the_scan_dir() {
        let (_a, first) = compile_source(HELLO);
        let (_b, second) = compile_source(HELLO);
        let (tmp, store) = open_store();
        let root = tmp.path().join("scans");
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
            matches!(&err, Error::MissingTask { scanner, task, .. } if scanner == "missing" && task == "nope"),
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
            &tmp.path().join("scans"),
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

    /// The attach phase runs a scanner's attachment functions against
    /// a dataset: `dataset().sessions()` lists the members, `attach`
    /// writes and links attachments with and without a target, every
    /// write is reported, a repeat is `unchanged`, and a failing
    /// function ends the phase with the earlier links intact.
    #[tokio::test]
    async fn attach_phase_writes_and_links_attachments_for_a_dataset() {
        use crate::attach::{AttachError, AttachEvent, attach};
        use gage_runtime2::Attached;
        use gage_store::AttachmentStore;

        const SCANNER: &str = r###"
            use gage::{attach, dataset, Files};

            pub const SCANNER = #{
                name: "attacher",
                description: "Attaches things",
                tasks: #{
                    cfg: #{ attaches: ["claude-config"], call: "attach_cfg" },
                    stack: #{ attaches: ["stack-files", "never-written"] },
                    check: #{ call: "run_check", attachments: #{ needs: ["claude-config"] } },
                },
            };

            pub async fn attach_cfg() {
                let a = attach(Files::include(["settings.json"]).root(ROOT))
                    .name("claude-config")
                    .await?;
                println!("{} {}", a.name.unwrap(), a.outcome);
            }

            pub async fn stack() {
                for s in dataset().sessions().await {
                    let project = s.attrs().await.project.unwrap();
                    log::info!("project {}", project);
                    let native = s.native().await?.unwrap();
                    println!("native {:?}", native.project_dir);
                    attach(Files::include(["CLAUDE.md"]).exclude(["nope"]).root(ROOT))
                        .name("stack-files")
                        .target(s)
                        .await?;
                }
                for (s, native) in dataset().sessions().native().await {
                    println!("natives {} {:?}", s.id, native?.project_dir);
                }
            }

            pub async fn run_check() {
                for s in gage::scan().sessions().await {
                    for a in s.attachments().name("stack-*").await? {
                        println!("{} {:?}", a.name.unwrap(), a.targets);
                    }
                }
                println!("{}", gage::scan().attachments().name("claude-config").await?.len());
            }
        "###;

        let (tmp, store) = open_store();
        let root = tmp.path().join("files");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("settings.json"), "{}").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "rules").unwrap();
        let source = SCANNER.replace("ROOT", &format!("{:?}", root.display().to_string()));
        // The session's project is a real directory the source's
        // registry records, so `native()` resolves it
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = std::fs::canonicalize(&work).unwrap();
        let (dataset_id, _sha, session_id) = seeded_dataset_for_project(
            tmp.path(),
            &store,
            r#"{"type":"user","uuid":"u1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            &work,
        );
        let (_dir, compiled) = compile_source(&source);
        let compiled = compiled.unwrap();
        assert_eq!(
            compiled
                .attach_tasks()
                .map(|t| (t.name.as_str(), t.call.as_str()))
                .collect::<Vec<_>>(),
            [("cfg", "attach_cfg"), ("stack", "stack")]
        );

        std::fs::write(
            tmp.path().join("claude").join(".claude.json"),
            format!(r#"{{"projects": {{"{}": {{}}}}}}"#, work.display()),
        )
        .unwrap();
        let mut events = Vec::new();
        attach(
            &store,
            &dataset_id,
            std::slice::from_ref(&compiled),
            claude_driver(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        let attached: Vec<&Attached> = events
            .iter()
            .filter_map(|e| match e {
                AttachEvent::Attached { attached, .. } => Some(attached),
                _ => None,
            })
            .collect();
        assert_eq!(attached.len(), 2, "{events:?}");
        assert_eq!(attached[0].name.as_deref(), Some("claude-config"));
        assert!(attached[0].targets.is_empty());
        assert_eq!(attached[0].outcome, "added");
        assert_eq!(attached[1].name.as_deref(), Some("stack-files"));
        assert_eq!(attached[1].targets, [format!("session:{session_id}")]);
        assert!(
            events.contains(&AttachEvent::Output(TaskOutput {
                scanner: "attacher".into(),
                task: "cfg".into(),
                output: Output::Println("claude-config added".into()),
            })),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AttachEvent::Output(TaskOutput { task, output: Output::Log { message, .. }, .. })
                    if task == "stack" && message.starts_with("project ")
            )),
            "{events:?}"
        );
        assert!(
            events.contains(&AttachEvent::Output(TaskOutput {
                scanner: "attacher".into(),
                task: "stack".into(),
                output: Output::Println(format!("native Some({:?})", work.display().to_string())),
            })),
            "{events:?}"
        );
        assert!(
            events.contains(&AttachEvent::Output(TaskOutput {
                scanner: "attacher".into(),
                task: "stack".into(),
                output: Output::Println(format!(
                    "natives {session_id} Some({:?})",
                    work.display().to_string()
                )),
            })),
            "{events:?}"
        );
        assert!(
            events.contains(&AttachEvent::Started {
                scanner: "attacher".into(),
                task: "cfg".into(),
            }),
            "{events:?}"
        );
        let warnings: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AttachEvent::Warning { task, message, .. } if task == "stack" => {
                    Some(message.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            warnings,
            ["declares 'never-written' in `attaches` but attached nothing by that name"]
        );
        let linked = DatasetStore::from(&store).attachments(&dataset_id).unwrap();
        assert_eq!(linked.len(), 2);
        let targeted = linked
            .iter()
            .find(|a| a.attrs.name.as_deref() == Some("stack-files"))
            .unwrap();
        let object = store.read_object(&targeted.commit_sha).unwrap();
        assert!(object.tree.links.contains_key("targets.link"));
        assert_eq!(
            AttachmentStore::from(&store)
                .files(&targeted.commit_sha)
                .unwrap()
                .len(),
            1
        );

        // A repeat changes nothing
        let mut again = Vec::new();
        attach(
            &store,
            &dataset_id,
            std::slice::from_ref(&compiled),
            claude_driver(),
            |e| again.push(e),
        )
        .await
        .unwrap();
        let outcomes: Vec<&str> = again
            .iter()
            .filter_map(|e| match e {
                AttachEvent::Attached { attached, .. } => Some(attached.outcome.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(outcomes, ["unchanged", "unchanged"]);

        // The scan side reads what the phase attached
        let dataset_sha = DatasetStore::from(&store)
            .get(&dataset_id)
            .unwrap()
            .commit_sha;
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
        };
        let mut events = Vec::new();
        let outcome = scan(
            &store,
            &config,
            std::slice::from_ref(&compiled),
            &CancellationToken::new(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        assert_eq!(outcome.attrs.tasks.failed, 0, "{events:?}");
        // The scan ran the scan task alone
        let started: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::TaskStarted { task, .. } => Some(task.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(started, ["check"]);
        let printed: Vec<&Output> = events
            .iter()
            .filter_map(|e| match e {
                Event::Output(TaskOutput { output, .. }) => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(
            printed,
            [
                &Output::Println(format!("stack-files [\"session:{session_id}\"]")),
                &Output::Println("1".into()),
            ]
        );

        // A function whose selection fails ends the phase
        const BROKEN: &str = r###"
            use gage::{attach, Files};
            pub const SCANNER = #{
                name: "broken",
                description: "Bad root",
                tasks: #{ bad: #{ attaches: ["x"] } },
            };
            pub async fn bad() {
                attach(Files::include(["x"]).root("/nonexistent/dir")).await?;
            }
        "###;
        let (_dir2, broken) = compile_source(BROKEN);
        let err = attach(
            &store,
            &dataset_id,
            &[broken.unwrap()],
            claude_driver(),
            |_| {},
        )
        .await
        .unwrap_err();
        match err {
            AttachError::Task {
                scanner,
                task,
                message,
            } => {
                assert_eq!((scanner.as_str(), task.as_str()), ("broken", "bad"));
                assert!(message.contains("/nonexistent/dir"), "{message}");
            }
            other => panic!("expected a task failure, got {other}"),
        }
        assert_eq!(
            DatasetStore::from(&store)
                .attachments(&dataset_id)
                .unwrap()
                .len(),
            2
        );
    }

    /// A declaration defect stops the scanner at compile with every
    /// problem labelled, before any task runs.
    #[test]
    fn declaration_problems_fail_compile_with_labelled_diagnostics() {
        const SCANNER: &str = r#"
            pub const SCANNER = #{
                name: "bad",
                description: "Defective declaration",
                tasks: #{
                    main: #{ call: run_main },
                    other: "nope",
                },
            };
            pub fn main() {}
        "#;
        let (_dir, compiled) = compile_source(SCANNER);
        let err = compiled.err().expect("the declaration has two problems");
        let Error::Invalid { name, diagnostics } = &err else {
            panic!("expected Error::Invalid, got {err}");
        };
        assert_eq!(name, "bad");
        assert!(
            diagnostics.contains("task 'main' field 'call' has unexpected type"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("task 'other' must be an object"),
            "{diagnostics}"
        );
        assert!(diagnostics.contains("run_main"), "{diagnostics}");
    }

    /// A dataset holding one `claude` session seeded from `jsonl`,
    /// under a project directory that need not exist. Returns the
    /// dataset id, its commit, and the session's Gage id.
    fn seeded_dataset(
        root: &std::path::Path,
        store: &Store,
        jsonl: &str,
    ) -> (String, String, String) {
        seeded_dataset_for_project(root, store, jsonl, std::path::Path::new("/home/alice/proj"))
    }

    /// As [`seeded_dataset`], with the session filed under
    /// `project_dir` as Claude encodes it.
    fn seeded_dataset_for_project(
        root: &std::path::Path,
        store: &Store,
        jsonl: &str,
        project_dir: &std::path::Path,
    ) -> (String, String, String) {
        use gage_session::Driver;
        use gage_store::SessionSpec;

        let claude = root.join("claude");
        let native_id = "11111111-2222-3333-4444-555555555555";
        let dir = claude
            .join("projects")
            .join(gage_claude::session::encode_project_dir(project_dir));
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
                    for m in s.messages().await {
                        println!("{}/{}: {}", m.type, m.subtype.unwrap_or("-"), m.text);
                    }
                    for m in s.messages().type("assistant").latest_first().await? {
                        println!("latest assistant: {}", m.text);
                    }
                    for m in s.messages().lines(2, 3).await {
                        println!("lines 2-3: {}", m.text);
                    }
                    for m in s.messages().latest_first().limit(1).await {
                        println!("limit 1: {}", m.text);
                    }
                    println!("{} entries", s.entries().await.len());
                    println!("{} entries limit 2", s.entries().limit(2).await.len());
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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

    /// `write_note` writes a note the apply creates and the scan links:
    /// the author is the task, `attrs.scan` is the scan, a session
    /// target pins the member commit, and bad lines are the scanner's
    /// error.
    #[tokio::test]
    async fn tasks_write_notes_that_the_scan_links() {
        use gage_store::NoteStore;

        const SCANNER: &str = r#"
            use gage::{Target, scan, write_note};

            pub const SCANNER = #{
                name: "notes",
                description: "Notes",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                for s in scan().sessions().await {
                    let n = write_note("thinking.empty", true)
                        .target(Target::session_line(s.id, 2))
                        .metadata(#{ model: "m" })
                        .await?;
                    println!("{} {} {:?}", n.name, n.author, n.target);
                    let n = write_note("comment", "whole")
                        .target(Target::session_lines(s.id, ""))
                        .await?;
                    println!("{:?}", n.target);
                    let n = write_note("comment", "ranged")
                        .target(Target::session_range(s.id, 1, 3))
                        .await?;
                    println!("{:?}", n.target);
                    let n = write_note("comment", "by-session").target(s).await?;
                    println!("{:?} {}", n.target, s.id);
                    match write_note("bad", 1).target(Target::session_line(s.id, 0)).await {
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
                &Output::Println("args: write_note target: invalid line selection: 0".into()),
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
            first.target_commit,
            Some(member_sha),
            "the note links the member commit the scan read"
        );
        assert_eq!(first.metadata, Some(serde_json::json!({"model": "m"})));
        assert!(
            !tmp.path().join("scans").join(&outcome.id).exists(),
            "the scan directory is removed after apply"
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
    async fn tasks_write_issues_that_cite_the_scans_notes_and_the_scan_links() {
        use gage_store::{IssueInput, IssueStatus, IssueStore, NoteStore};

        const SCANNER: &str = r###"
            use gage::{Target, scan, write_issue, write_note};

            pub const SCANNER = #{
                name: "issues",
                description: "Issues",
                tasks: #{
                    main: #{
                        notes: #{ writes: #{ "finding.code": "A code finding" } },
                    },
                },
            };

            pub async fn main() {
                // The scan's issues: none yet, and never the store's
                let before = scan().issues().await?;
                println!("before {}", before.len());
                let note = None;
                for s in scan().sessions().await {
                    note = Some(write_note("finding.code", "retry loop")
                        .target(Target::session_line(s.id, 2))
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
                let all = scan().issues().await?;
                let pending = scan().issues().status("pending").await?;
                let named = scan().issues().name(["findings", "prior"]).status(["open", "pending"]).await?;
                println!("after {} {} {} {:?}", all.len(), pending.len(), named.len(), all[0].evidence);
                match scan().issues().status("bogus").await {
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
        // An issue already in the store is outside the scan's scope
        let prior = IssueStore::from(&store)
            .create(IssueInput {
                name: "prior",
                title: "Prior",
                description: None,
                author: "user:t",
                status: IssueStatus::Open,
                evidence: &[],
                key: None,
            })
            .unwrap();

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
            "the issue links the commit of the note the scan wrote"
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
                &Output::Println("before 0".into()),
                &Output::Println(format!(
                    "findings pending task:issues:main [\"{note_id}\"] Some(\"## Summary\\n\\nRetries.\")"
                )),
                &Output::Println(format!("session-retention open 1 None")),
                &Output::Println("args: write_issue evidence: object not found: nosuchnote".into()),
                &Output::Println(
                    "args: evidence must be a note id, a Note, or a list of either".into()
                ),
                &Output::Println(format!("after 2 1 1 [\"{note_id}\"]")),
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

        // The stored scan's scope serves the same tables over that scan
        // alone: the prior issue is outside it
        let scoped = gage_query2::ContextBuilder::new(Some(Arc::new(Mutex::new(
            Store::open(store.path()).unwrap(),
        ))))
        .scope(gage_query2::ScanScope::stored(outcome.id.clone()))
        .build()
        .await;
        let count = |sql: &'static str| {
            let ctx = scoped.clone();
            async move {
                let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
                batches[0]
                    .column(0)
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                    .unwrap()
                    .value(0)
            }
        };
        assert_eq!(count("SELECT COUNT(*) FROM issue").await, 2);
        assert_eq!(
            count("SELECT COUNT(*) FROM issue WHERE name = 'prior'").await,
            0
        );
        assert_eq!(count("SELECT COUNT(*) FROM note").await, 1);
        assert_eq!(
            count("SELECT COUNT(*) FROM scan_note WHERE NOT carried").await,
            1
        );
        assert_eq!(count("SELECT COUNT(*) FROM session").await, 1);
        assert_eq!(count("SELECT COUNT(*) FROM scan").await, 1);
        assert_eq!(count("SELECT COUNT(*) FROM issue_evidence").await, 2);
        assert_eq!(
            count("SELECT COUNT(*) FROM note WHERE commit IS NOT NULL").await,
            1,
            "a stored scan's notes have commits"
        );
        let docs = scoped
            .sql("SELECT doc, written_by FROM note_doc WHERE note_name = 'finding.code'")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let text = |i: usize| {
            docs[0]
                .column(i)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0)
                .to_string()
        };
        assert_eq!(
            (text(0), text(1)),
            ("A code finding".to_string(), "issues:main".to_string()),
            "the scan's plan carries the docs of the notes its tasks declare"
        );
    }

    /// `key(k)` stores an identity key. A later scan's write under
    /// the same key is a new commit of the
    /// live issue, with the write's whole state, and the scan links
    /// that commit. A second write in one scan replaces the first.
    #[tokio::test]
    async fn issues_written_under_a_key_replace_the_live_issue() {
        use gage_store::{ChangeEvent, IssueStatus, IssueStore};

        const SCANNER: &str = r###"
            use gage::{Target, scan, write_issue, write_note};

            pub const SCANNER = #{
                name: "replace",
                description: "Replace",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                let note = None;
                for s in scan().sessions().await {
                    note = Some(write_note("empty-thinking", true)
                        .target(Target::session_line(s.id, 1))
                        .await?);
                }
                let note = note.unwrap();
                let first = write_issue("hidden-thinking", "Hidden", "first")
                    .key("hidden-thinking")
                    .evidence(note)
                    .await?;
                let i = write_issue("hidden-thinking", "Hidden", "second")
                    .key("hidden-thinking")
                    .evidence(note)
                    .await?;
                println!("{} {} {:?} {}", i.id == first.id, i.status, i.key, i.evidence.len());
                let k = write_issue("per-session", "Per session", "")
                    .key(("per-session", 7))
                    .await?;
                println!("{:?}", k.key);
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
            let root = tmp.path().join(format!("scans{n}"));
            let dataset_sha = dataset_sha.clone();
            async move {
                let mut events = Vec::new();
                let config = ScanConfig {
                    scans_dir: &root,
                    gage_version: "test-version",
                    dataset: Some(&dataset_sha),
                    jobs: 1,
                    driver: claude_driver(),
                    invalidate: false,
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
        assert_eq!(hidden.key.as_deref(), Some("hidden-thinking"));
        assert_eq!(
            hidden.description.as_deref(),
            Some("second"),
            "the second write in the scan replaced the first"
        );
        assert_eq!(hidden.changes.len(), 1);
        assert_eq!(hidden.evidence, [record.content.notes[0].clone()]);
        let per_session = written[1].clone();
        assert_eq!(per_session.key.as_deref(), Some("per-session:7"));

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

    /// A selection without a root is under the scanner's directory,
    /// and a relative root is resolved against it.
    #[tokio::test]
    async fn attach_root_defaults_to_the_scanner_directory() {
        use crate::attach::{AttachEvent, attach};
        use gage_store::AttachmentStore;

        const SCANNER: &str = r###"
            use gage::{attach, Files};
            pub const SCANNER = #{
                name: "self-attacher",
                description: "Attaches its own source",
                tasks: #{
                    src: #{ attaches: ["src"] },
                    sub: #{ attaches: ["sub"] },
                },
            };
            pub async fn src() {
                attach(Files::include(["scanner.rn"])).name("src").await?;
            }
            pub async fn sub() {
                attach(Files::include(["*.txt"]).root("data")).name("sub").await?;
            }
        "###;
        let (tmp, store) = open_store();
        let (dir, compiled) = compile_source(SCANNER);
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data").join("a.txt"), "a").unwrap();
        let dataset_id = DatasetStore::from(&store).create().unwrap();
        let mut events = Vec::new();
        attach(
            &store,
            &dataset_id,
            &[compiled.unwrap()],
            claude_driver(),
            |e| events.push(e),
        )
        .await
        .unwrap();
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AttachEvent::Attached { attached, .. } => Some(attached.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "{events:?}");
        let attachments = AttachmentStore::from(&store);
        let scanner_dir = std::fs::canonicalize(dir.path()).unwrap();
        let src = attachments.get(&ids[0]).unwrap();
        assert_eq!(src.attrs.root, scanner_dir);
        assert_eq!(
            attachments
                .files(&src.commit_sha)
                .unwrap()
                .iter()
                .map(|f| f.key.as_str())
                .collect::<Vec<_>>(),
            ["scanner.rn"]
        );
        let sub = attachments.get(&ids[1]).unwrap();
        assert_eq!(sub.attrs.root, scanner_dir.join("data"));
        assert_eq!(sub.attrs.file_count, 1);
        drop(tmp);
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
                let a = scan().attachments().name("cfg").await?.next().unwrap();
                println!("{} {:?}", a.name.unwrap(), a.files().await);
                let f = a.file("settings.json").await.unwrap();
                println!("{:?}", f.json()?.get("cleanupPeriodDays"));
                println!("{}", a.file("missing.json").await.is_none());
                println!("{}", scan().attachments().name("nope").await?.len());
                println!("{}", scan().attachments().name("c*").await?.len());
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
        let mut ids = Vec::new();
        for name in ["cfg", "other"] {
            let added = attachments
                .add(&AttachmentSpec {
                    name: Some(name),
                    key: None,
                    targets: &[],
                    root: &root,
                    includes: &pats,
                    excludes: &[],
                })
                .unwrap();
            let dataset = datasets.create().unwrap();
            datasets
                .attachments_link(&dataset, &[added.id.clone()])
                .unwrap();
            ids.push(added.id);
        }
        // One dataset holds both; scan that one
        let dataset = datasets.create().unwrap();
        datasets.attachments_link(&dataset, &ids).unwrap();
        let dataset_sha = datasets.get(&dataset).unwrap().commit_sha;

        let (_dir, compiled) = compile_source(SCANNER);
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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
                &Output::Println("0".into()),
                &Output::Println("1".into()),
            ]
        );
    }

    /// `key(k).once()` writes once: a second scan finds the issue live
    /// in the store, in any status, and returns it without a write;
    /// a second write in one scan returns the first. `once()` without
    /// a key is an `Args` error.
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
                    .key("retention")
                    .once()
                    .await?;
                let again = write_issue("retention", "Retention", "second")
                    .key("retention")
                    .once()
                    .await?;
                println!("{} {} {:?} {:?}", again.id == first.id, again.status, again.description, again.key);
                match write_issue("retention", "Retention", "third").once().await {
                    Err(gage::Error::Args(m)) => println!("args: {m}"),
                    other => println!("unexpected: {other:?}"),
                }
                Ok(())
            }
        "###;

        let (tmp, store) = open_store();
        let (_dir, compiled) = compile_source(SCANNER);
        let compiled = compiled.unwrap();
        let run = |n: u32, expected: &'static str| {
            let store = &store;
            let compiled = &compiled;
            let root = tmp.path().join(format!("scans{n}"));
            async move {
                let mut events = Vec::new();
                let config = ScanConfig {
                    scans_dir: &root,
                    gage_version: "test-version",
                    dataset: None,
                    jobs: 1,
                    driver: claude_driver(),
                    invalidate: false,
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
                        &Output::Println(expected.into()),
                        &Output::Println("args: write_issue: once() requires key()".into()),
                    ]
                );
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate: false,
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

    const NATIVE_SCANNER: &str = r#"
        use gage::scan;

        pub const SCANNER = #{
            name: "nat",
            description: "Native sessions",
            tasks: #{ main: #{} },
        };

        pub async fn main() {
            for (s, native) in scan().sessions().native().await {
                match native {
                    Ok(native) => println!("all {} {:?}", s.id, native.project_dir),
                    Err(gage::Error::Driver(m)) => println!("all driver: {m}"),
                    other => println!("unexpected: {other:?}"),
                }
            }
            let s = scan().sessions().await.next().unwrap();
            match s.native().await {
                Ok(native) => println!("one {:?}", native.map(|n| n.project_dir)),
                Err(gage::Error::Driver(m)) => println!("one driver: {m}"),
                other => println!("unexpected: {other:?}"),
            }
            Ok(())
        }
    "#;

    /// `sessions().native()` pairs each member with the result of
    /// resolving its native session, and `session.native()` is that
    /// result alone. A driver failure, here an unreadable project
    /// registry, is `Error::Driver`: the session's entry in the
    /// batch, the await of the single read.
    #[tokio::test]
    async fn native_sessions_resolve_and_surface_driver_failures() {
        let (tmp, store) = open_store();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = std::fs::canonicalize(&work).unwrap();
        let (_dataset_id, dataset_sha, session_id) =
            seeded_dataset_for_project(tmp.path(), &store, ONE_LINE, &work);
        let registry = tmp.path().join("claude").join(".claude.json");
        let (_dir, compiled) = compile_source(NATIVE_SCANNER);
        let compiled = compiled.unwrap();

        std::fs::write(
            &registry,
            format!(r#"{{"projects": {{"{}": {{}}}}}}"#, work.display()),
        )
        .unwrap();
        let (_, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha).await;
        let dir = work.display().to_string();
        assert_eq!(
            printed,
            [
                format!("all {session_id} Some({dir:?})"),
                format!("one Some(Some({dir:?}))"),
            ]
        );

        std::fs::write(&registry, "not json").unwrap();
        let (_, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha).await;
        assert_eq!(printed.len(), 2, "{printed:?}");
        for (line, prefix) in printed.iter().zip(["all driver: ", "one driver: "]) {
            let rest = line
                .strip_prefix(prefix)
                .unwrap_or_else(|| panic!("{line:?} lacks {prefix:?}"));
            assert!(rest.starts_with("project path of "), "{line:?}");
            assert!(rest.contains("reading project registry"), "{line:?}");
        }
    }

    const WATERMARK_SCANNER: &str = r#"
        use gage::{Target, Mark, carry_forward_notes, scan, watermark, write_note};

        pub const SCANNER = #{
            name: "wm",
            description: "Watermarks",
            tasks: #{ main: #{} },
        };

        const KEY = ("wm", "main", 1);

        pub async fn main() {
            let carried = carry_forward_notes(KEY).await?;
            println!("carried {carried}");
            for (s, hwm) in scan().sessions().hwm(KEY).await? {
                println!("hwm {hwm} of {}", s.line_count);
            }
            for (s, (start, end)) in scan().sessions().unseen(KEY).await? {
                println!("unseen {start}-{end}");
                let note = write_note("seen", format!("{start}-{end}"))
                    .target(Target::session_range(s.id, start, end))
                    .carry_forward_key(KEY)
                    .await?;
                write_note("untagged", "x").target(s).await?;
                watermark(Mark::session(s), KEY).await?;
                watermark(Mark::note(note), KEY).await?;
            }
            for (n, hwm) in scan().notes().name("seen").hwm(KEY).await? {
                println!("note {} hwm {hwm}", n.value);
            }
            let unseen = scan().notes().name("seen").unseen(KEY).await?;
            println!("notes unseen {}", unseen.len());
            match watermark(Mark::session("not-a-member"), KEY).await {
                Err(gage::Error::Args(m)) => println!("args: {m}"),
                other => println!("unexpected: {other:?}"),
            }
            match watermark(Mark::note("not-a-note"), KEY).await {
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
        run_watermark_scan_with(tmp, store, compiled, dataset_sha, false).await
    }

    async fn run_watermark_scan_with(
        tmp: &TempDir,
        store: &Store,
        compiled: &CompiledScanner,
        dataset_sha: &str,
        invalidate: bool,
    ) -> (ScanOutcome, Vec<String>) {
        let mut events = Vec::new();
        let config = ScanConfig {
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(dataset_sha),
            jobs: 1,
            driver: claude_driver(),
            invalidate,
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

    /// The first scan finds the whole session unseen, watermarks it
    /// and the note it wrote; a second scan of the same dataset
    /// commit finds nothing unseen, carries the tagged note, and
    /// finds it marked; a scan of the grown session finds the
    /// appended lines unseen and still carries the note written
    /// against the earlier commit; a scan of the earlier dataset
    /// commit again ignores the later scan's watermark and note,
    /// which sit on a descendant commit; an invalidating scan of the
    /// grown session finds the whole session unseen, carries nothing,
    /// rewrites the note, and watermarks it.
    #[tokio::test]
    async fn watermarks_resume_grown_sessions_and_carry_tagged_notes() {
        let (tmp, store) = open_store();
        let (dataset_id, dataset_sha_1, session_id) = seeded_dataset(tmp.path(), &store, ONE_LINE);
        let (_dir, compiled) = compile_source(WATERMARK_SCANNER);
        let compiled = compiled.unwrap();
        let args_lines = [
            "args: session not-a-member is not a member of the scan".to_string(),
            "args: note not-a-note was neither written nor carried by the scan".to_string(),
            "args: key must be non-empty and must not contain '/': \"a/b\"".to_string(),
        ];

        let (first, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 0".to_string(),
                "hwm 0 of 1".to_string(),
                "unseen 1-1".to_string(),
                "note 1-1 hwm 0".to_string(),
                "notes unseen 1".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
                args_lines[2].clone(),
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
        let notes = NoteStore::from(&store);
        let tagged_1 = first_record
            .content
            .notes
            .iter()
            .map(|sha| (sha.clone(), notes.at_commit(sha).unwrap()))
            .find(|(_, n)| n.name == "seen")
            .expect("the scanner wrote the tagged note");
        assert_eq!(tagged_1.1.carry_forward_key.as_deref(), Some("wm:main:1"));
        assert_eq!(
            tagged_1.1.target.as_deref(),
            Some(format!("session:{session_id}#1-1").as_str())
        );
        let mut expected = vec![
            gage_store::Watermark {
                oid: session_id.clone(),
                key: "wm:main:1".into(),
                version: member_sha_1.clone(),
                mark: 1,
            },
            gage_store::Watermark {
                oid: tagged_1.1.id.clone(),
                key: "wm:main:1".into(),
                version: tagged_1.0.clone(),
                mark: 1,
            },
        ];
        expected.sort_by(|a, b| a.oid.cmp(&b.oid));
        assert_eq!(
            first_record.content.watermarks, expected,
            "the watermark on the note the scan wrote resolved to its commit at apply"
        );

        let (second, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "hwm 1 of 1".to_string(),
                "note 1-1 hwm 1".to_string(),
                "notes unseen 0".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
                args_lines[2].clone(),
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
                "hwm 1 of 3".to_string(),
                "unseen 2-3".to_string(),
                "note 1-1 hwm 1".to_string(),
                "note 2-3 hwm 0".to_string(),
                "notes unseen 1".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
                args_lines[2].clone(),
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
        let session_mark = |record: &gage_store::ScanRecord| {
            record
                .content
                .watermarks
                .iter()
                .find(|w| w.oid == session_id)
                .cloned()
                .expect("the scan watermarked its session")
        };
        assert_eq!(session_mark(&third_record).version, member_sha_2);
        assert_eq!(session_mark(&third_record).mark, 3);

        let (fourth, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "hwm 1 of 1".to_string(),
                "note 1-1 hwm 1".to_string(),
                "notes unseen 0".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
                args_lines[2].clone(),
            ]
        );
        let fourth_record = scans.get(&fourth.id).unwrap();
        assert_eq!(
            fourth_record.content.notes_carried,
            [tagged_1.0.clone()],
            "the third scan's note targets a descendant commit and is not carried"
        );

        let (fifth, printed) =
            run_watermark_scan_with(&tmp, &store, &compiled, &dataset_sha_2, true).await;
        assert_eq!(
            printed,
            [
                "carried 0".to_string(),
                "hwm 0 of 3".to_string(),
                "unseen 1-3".to_string(),
                "note 1-3 hwm 0".to_string(),
                "notes unseen 1".to_string(),
                args_lines[0].clone(),
                args_lines[1].clone(),
                args_lines[2].clone(),
            ]
        );
        let fifth_record = scans.get(&fifth.id).unwrap();
        assert_eq!(fifth_record.content.notes.len(), 2);
        assert!(fifth_record.content.notes_carried.is_empty());
        assert_eq!(session_mark(&fifth_record).version, member_sha_2);
        assert_eq!(session_mark(&fifth_record).mark, 3);
        let tagged_5 = fifth_record
            .content
            .notes
            .iter()
            .map(|sha| notes.at_commit(sha).unwrap())
            .find(|n| n.name == "seen")
            .expect("the invalidating scan rewrote the tagged note");
        assert_eq!(
            tagged_5.target.as_deref(),
            Some(format!("session:{session_id}#1-3").as_str())
        );

        let ctx = gage_query2::ContextBuilder::new(Some(Arc::new(Mutex::new(
            Store::open(store.path()).unwrap(),
        ))))
        .build()
        .await;
        let batches = ctx
            .sql("SELECT scan_id, version, mark FROM scan_watermark")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(
            rows, 6,
            "the first, third, and fifth scans each hold a session and a note watermark"
        );
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

    /// A scan running only a consumer carries the notes a writer left
    /// for its sessions by name, whatever key they carry, and sees
    /// them through `scan().notes()`; a name no note matches carries
    /// nothing.
    #[tokio::test]
    async fn a_consumer_carries_a_writers_notes_by_name() {
        const CONSUMER: &str = r#"
            use gage::{carry_forward_notes_named, scan};

            pub const SCANNER = #{
                name: "consumer",
                description: "Consumer",
                tasks: #{ main: #{} },
            };

            pub async fn main() {
                println!("carried {}", carry_forward_notes_named("se*").await?);
                println!("again {}", carry_forward_notes_named("seen").await?);
                println!("other {}", carry_forward_notes_named("nothing-*").await?);
                let notes = scan().notes().name("seen").await?;
                println!("notes {} {}", notes.len(), notes[0].value);
                Ok(())
            }
        "#;

        let (tmp, store) = open_store();
        let (_dataset_id, dataset_sha, _session_id) = seeded_dataset(tmp.path(), &store, ONE_LINE);
        let (_dir, writer) = compile_source(WATERMARK_SCANNER);
        run_watermark_scan(&tmp, &store, &writer.unwrap(), &dataset_sha).await;

        let (_dir, consumer) = compile_source(CONSUMER);
        let (outcome, printed) =
            run_watermark_scan(&tmp, &store, &consumer.unwrap(), &dataset_sha).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "again 0".to_string(),
                "other 0".to_string(),
                "notes 1 1-1".to_string(),
            ]
        );
        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert_eq!(record.content.notes_carried.len(), 1);
        assert!(record.content.notes.is_empty());
    }

    const ATTACHMENT_MARK_SCANNER: &str = r#"
        use gage::{Mark, carry_forward_notes, scan, watermark, write_note};

        pub const SCANNER = #{
            name: "am",
            description: "Attachment marks",
            tasks: #{ main: #{} },
        };

        const KEY = ("am", "main", 1);

        pub async fn main() {
            let carried = carry_forward_notes(KEY).await?;
            println!("carried {carried}");
            for (a, hwm) in scan().attachments().names(["cfg", "other"]).hwm(KEY).await? {
                println!("hwm {} {hwm}", a.name.unwrap());
            }
            for a in scan().attachments().name("cfg").unseen(KEY).await? {
                println!("unseen {}", a.name.unwrap());
                write_note("summary", a.digest.unwrap())
                    .target(a)
                    .carry_forward_key(KEY)
                    .await?;
                watermark(Mark::attachment(a), KEY).await?;
            }
            for n in scan().notes().name("summary").await? {
                println!("note {} {}", n.target.unwrap().starts_with("attachment:"), n.value);
            }
            match watermark(Mark::attachment("not-in-dataset"), KEY).await {
                Err(gage::Error::Args(m)) => println!("args: {m}"),
                other => println!("unexpected: {other:?}"),
            }
            Ok(())
        }
    "#;

    /// An attachment's mark and its notes follow the content digest.
    /// The first scan finds the attachment unseen, writes a note
    /// against it, and marks it at its digest; a second scan of the
    /// same dataset finds it marked and carries the note; a
    /// target-only edit of the attachment keeps the digest, so a scan
    /// of the re-linked dataset still finds it marked and carries the
    /// note; a content edit changes the digest, so the next scan
    /// finds it unseen, carries nothing, and rewrites the note; and a
    /// scan after that carries only the new note.
    #[tokio::test]
    async fn attachment_marks_and_carries_follow_the_content_digest() {
        use gage_store::{AttachmentSpec, AttachmentStore};

        let (tmp, store) = open_store();
        let root = tmp.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("settings.json"), "{\"a\": 1}").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "rules").unwrap();
        let attachments = AttachmentStore::from(&store);
        let datasets = DatasetStore::from(&store);
        let cfg_pats = ["settings.json".to_string()];
        let other_pats = ["CLAUDE.md".to_string()];
        let cfg_spec = AttachmentSpec {
            name: Some("cfg"),
            key: None,
            targets: &[],
            root: &root,
            includes: &cfg_pats,
            excludes: &[],
        };
        let cfg = attachments.add(&cfg_spec).unwrap();
        let other = attachments
            .add(&AttachmentSpec {
                name: Some("other"),
                includes: &other_pats,
                ..cfg_spec.clone()
            })
            .unwrap();
        let dataset = datasets.create().unwrap();
        datasets
            .attachments_link(&dataset, &[cfg.id.clone(), other.id.clone()])
            .unwrap();
        let dataset_sha_1 = datasets.get(&dataset).unwrap().commit_sha;
        let digest_1 = attachments.get(&cfg.id).unwrap().attrs.digest.unwrap();
        let (_dir, compiled) = compile_source(ATTACHMENT_MARK_SCANNER);
        let compiled = compiled.unwrap();
        let args_line = "args: attachment not-in-dataset is not in the scan's dataset".to_string();
        let key = "am:main:1";

        let (first, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 0".to_string(),
                "hwm cfg 0".to_string(),
                "hwm other 0".to_string(),
                "unseen cfg".to_string(),
                format!("note true {digest_1}"),
                args_line.clone(),
            ]
        );
        let scans = ScanStore::from(&store);
        let first_record = scans.get(&first.id).unwrap();
        assert_eq!(first_record.content.notes.len(), 1);
        assert_eq!(
            first_record.content.watermarks,
            [gage_store::Watermark {
                oid: cfg.id.clone(),
                key: key.into(),
                version: digest_1.clone(),
                mark: 1,
            }]
        );
        let notes = NoteStore::from(&store);
        let note_1 = notes.at_commit(&first_record.content.notes[0]).unwrap();
        assert_eq!(
            note_1.target.as_deref(),
            Some(format!("attachment:{}", cfg.id).as_str())
        );
        assert_eq!(
            note_1.target_commit.as_deref(),
            Some(cfg.commit_sha.as_str())
        );

        let (second, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_1).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "hwm cfg 1".to_string(),
                "hwm other 0".to_string(),
                format!("note true {digest_1}"),
                args_line.clone(),
            ]
        );
        let second_record = scans.get(&second.id).unwrap();
        assert!(second_record.content.notes.is_empty());
        assert!(second_record.content.watermarks.is_empty());
        assert_eq!(
            second_record.content.notes_carried,
            first_record.content.notes
        );

        // A target-only edit is a new attachment commit with the same
        // digest: still marked, note still carried
        let target_dataset = datasets.create().unwrap();
        let retargeted = attachments
            .add(&AttachmentSpec {
                targets: &[format!("dataset:{target_dataset}")],
                ..cfg_spec.clone()
            })
            .unwrap();
        assert_eq!(retargeted.outcome, gage_store::AttachmentOutcome::Updated);
        assert_eq!(
            attachments.get(&cfg.id).unwrap().attrs.digest.as_deref(),
            Some(digest_1.as_str())
        );
        datasets
            .attachments_link(&dataset, &[cfg.id.clone()])
            .unwrap();
        let dataset_sha_2 = datasets.get(&dataset).unwrap().commit_sha;
        assert_ne!(dataset_sha_2, dataset_sha_1);
        let (third, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_2).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "hwm cfg 1".to_string(),
                "hwm other 0".to_string(),
                format!("note true {digest_1}"),
                args_line.clone(),
            ]
        );
        let third_record = scans.get(&third.id).unwrap();
        assert!(third_record.content.notes.is_empty());
        assert!(third_record.content.watermarks.is_empty());
        assert_eq!(
            third_record.content.notes_carried,
            first_record.content.notes
        );

        // A content edit changes the digest: unseen again, the earlier
        // note is not carried, and the new note replaces it
        std::fs::write(root.join("settings.json"), "{\"a\": 2}").unwrap();
        let edited = attachments.add(&cfg_spec).unwrap();
        assert_eq!(edited.outcome, gage_store::AttachmentOutcome::Updated);
        let digest_2 = attachments.get(&cfg.id).unwrap().attrs.digest.unwrap();
        assert_ne!(digest_2, digest_1);
        datasets
            .attachments_link(&dataset, &[cfg.id.clone()])
            .unwrap();
        let dataset_sha_3 = datasets.get(&dataset).unwrap().commit_sha;
        let (fourth, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_3).await;
        assert_eq!(
            printed,
            [
                "carried 0".to_string(),
                "hwm cfg 0".to_string(),
                "hwm other 0".to_string(),
                "unseen cfg".to_string(),
                format!("note true {digest_2}"),
                args_line.clone(),
            ]
        );
        let fourth_record = scans.get(&fourth.id).unwrap();
        assert_eq!(fourth_record.content.notes.len(), 1);
        assert!(fourth_record.content.notes_carried.is_empty());
        assert_eq!(
            fourth_record.content.watermarks,
            [gage_store::Watermark {
                oid: cfg.id.clone(),
                key: key.into(),
                version: digest_2.clone(),
                mark: 1,
            }]
        );

        let (fifth, printed) = run_watermark_scan(&tmp, &store, &compiled, &dataset_sha_3).await;
        assert_eq!(
            printed,
            [
                "carried 1".to_string(),
                "hwm cfg 1".to_string(),
                "hwm other 0".to_string(),
                format!("note true {digest_2}"),
                args_line,
            ]
        );
        let fifth_record = scans.get(&fifth.id).unwrap();
        assert_eq!(
            fifth_record.content.notes_carried, fourth_record.content.notes,
            "only the note written at the current digest is carried"
        );

        let ctx = gage_query2::ContextBuilder::new(Some(Arc::new(Mutex::new(
            Store::open(store.path()).unwrap(),
        ))))
        .build()
        .await;
        let carried = ctx
            .sql(&format!(
                "SELECT note_id FROM scan_note WHERE scan_id = '{}' AND carried",
                fifth.id
            ))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let note_ids: Vec<String> = carried
            .iter()
            .flat_map(|b| {
                let col = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                (0..b.num_rows()).map(move |i| col.value(i).to_string())
            })
            .collect();
        let note_4 = notes.at_commit(&fourth_record.content.notes[0]).unwrap();
        assert_eq!(note_ids, [note_4.id]);
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
            &tmp.path().join("scans"),
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
            events[1],
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
        assert_eq!(
            events.last(),
            Some(&Event::Summary {
                id: outcome.id.clone(),
                attrs: outcome.attrs.clone(),
            })
        );
        assert_eq!(
            summary_line(short_uuid(&outcome.id), &outcome.attrs),
            format!(
                "Scan {} completed: 3 tasks: 2 completed, 1 failed",
                short_uuid(&outcome.id)
            )
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
                "unmatched_note_wants": ["nobody"],
                "note_writes": { "y": "the y note" }
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: None,
            jobs: 2,
            driver: claude_driver(),
            invalidate: false,
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
            (1, 2),
            "both ready tasks start right after the start, before either finishes: {events:?}"
        );
        assert!(
            started("c") > finished("a"),
            "c waits for a, which writes what it wants: {events:?}"
        );
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
                    write_note("a", now).target(s).await?;
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
            scans_dir: &tmp.path().join("scans"),
            gage_version: "test-version",
            dataset: Some(&dataset_sha),
            jobs: 2,
            driver: claude_driver(),
            invalidate: false,
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
