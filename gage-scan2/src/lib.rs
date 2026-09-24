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
//! the `gage-runtime2` context, runs their tasks, and records the run
//! in [`staging`] and then the store. The runtime is a pure event
//! emitter: [`scan`] hands each [`Event`] to the caller's sink, which
//! owns rendering.
//!
//! Rule: code in this crate and in `gage-runtime2` never calls
//! `println!` or `eprintln!`. Every line meant for a person is emitted
//! as [`Event::Scan`] output, which the scan writes to its `logs/out`
//! or `logs/err` before the sink shows it, so the stored record and
//! the terminal hold the same text. Runtime diagnostics go through
//! `tracing` and reach the record through [`trace`].

pub mod staging;
pub mod trace;

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_registry::scanner::ScannerDef;
use gage_runtime2::source::{SourceError, SourceFile, source_files};
use gage_runtime2::{OUTPUT_TX, Output};
use gage_scan::error::render_task_error;
use gage_store::{ScanAttrs, ScanStore, Store, StoreError, TaskAttrs, TaskCounts, TaskStatus};
use rune::runtime::{RuntimeContext, Unit, Value, VmError};
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources, Vm};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::staging::{Logs, ScannerPlan, Staging, State};
use crate::trace::{LOG_SCOPE, LogScope};

/// One item of run output, in the order it happened.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Task output
    Output(Output),
    /// The scan's own output for a person, already recorded in the
    /// scan's `logs/`
    Scan(ScanOutput),
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
    /// Declared task names, sorted by name
    tasks: Vec<String>,
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
}

/// Compile a scanner and verify that every declared task maps to a
/// function of the same name. A scanner that fails here is a full
/// stop for the caller: nothing has run yet.
pub fn compile(def: &ScannerDef) -> Result<CompiledScanner, Error> {
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

    Ok(CompiledScanner {
        name: def.name.clone(),
        tasks: def.tasks.keys().cloned().collect(),
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

/// Where a scan stages and what it records about its runtime.
pub struct ScanConfig<'a> {
    /// The staging root, `staging/` under Gage home in production
    pub staging_root: &'a std::path::Path,
    /// The Gage build version; the scan records `gage <version>` as
    /// its `runtime`
    pub gage_version: &'a str,
}

/// What a finished scan wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub id: String,
    pub commit_sha: String,
    pub attrs: ScanAttrs,
}

/// Run every task of every scanner, in order, one at a time, and
/// record the scan in `store`.
///
/// The scan is staged under `config.staging_root/<id>/` while it runs
/// (see [`staging`]) and applied to the store at its terminal state,
/// after which the staging directory is removed. Output and task
/// status reach `on_event` as they happen. A failed task is recorded
/// and the run continues with the next task. Cancelling `cancel`
/// abandons the running task at its next await point, marks it and
/// every task not yet started `canceled`, and applies what ran.
pub async fn scan(
    store: &Store,
    config: &ScanConfig<'_>,
    scanners: &[CompiledScanner],
    cancel: &CancellationToken,
    on_event: impl FnMut(Event),
) -> Result<ScanOutcome, ScanError> {
    let plan = plan_tasks(scanners)?;
    let id = new_uuid();
    let scanner_plans: Vec<ScannerPlan<'_>> = scanners
        .iter()
        .map(|s| ScannerPlan {
            name: &s.name,
            tasks: &s.tasks,
            sources: &s.source_files,
        })
        .collect();
    let staging = Staging::create(config.staging_root, &id, &scanner_plans)?;
    trace::install_panic_hook();
    let scope = LogScope {
        scan_dir: staging.scan_dir(),
        task: None,
        failure: Arc::new(Mutex::new(None)),
    };
    let run = Run {
        id,
        config,
        scanners,
        plan,
        staging,
        cancel,
        scope: scope.clone(),
        on_event,
    };
    LOG_SCOPE.scope(scope, run.execute(store)).await
}

/// One scan in progress.
struct Run<'a, F: FnMut(Event)> {
    id: String,
    config: &'a ScanConfig<'a>,
    scanners: &'a [CompiledScanner],
    plan: Vec<(String, String)>,
    staging: Staging,
    cancel: &'a CancellationToken,
    scope: LogScope,
    on_event: F,
}

impl<F: FnMut(Event)> Run<'_, F> {
    async fn execute(mut self, store: &Store) -> Result<ScanOutcome, ScanError> {
        tracing::info!("scan {} started with {} tasks", self.id, self.plan.len());
        let mut scan_logs = self.staging.scan_logs();
        let started = now_ms();
        let mut counts = TaskCounts {
            total: self.plan.len(),
            ..TaskCounts::default()
        };
        let mut canceled = false;
        let plan = std::mem::take(&mut self.plan);
        for (scanner, task) in &plan {
            if self.cancel.is_cancelled() {
                if !canceled {
                    self.say(&mut scan_logs, ScanOutput::Err("scan canceled\n".into()))?;
                }
                canceled = true;
                self.staging.write_task(
                    scanner,
                    task,
                    &task_attrs(TaskStatus::Canceled, None, None),
                )?;
                (self.on_event)(Event::TaskFinished {
                    scanner: scanner.clone(),
                    task: task.clone(),
                    status: TaskStatus::Canceled,
                    error: None,
                });
                continue;
            }
            let compiled = self
                .scanners
                .iter()
                .find(|s| &s.name == scanner)
                .expect("plan names a compiled scanner");
            let task_started = now_ms();
            self.staging.write_task(
                scanner,
                task,
                &task_attrs(TaskStatus::Started, Some(task_started), None),
            )?;
            (self.on_event)(Event::TaskStarted {
                scanner: scanner.clone(),
                task: task.clone(),
            });
            let mut logs = self.staging.task_logs(scanner, task);
            let task_scope = LogScope {
                task: Some((scanner.clone(), task.clone())),
                ..self.scope.clone()
            };
            let outcome = LOG_SCOPE
                .scope(
                    task_scope,
                    run_task(compiled, task, self.cancel, &mut logs, &mut self.on_event),
                )
                .await?;
            let (status, error) = match outcome {
                TaskOutcome::Completed => (TaskStatus::Completed, None),
                TaskOutcome::Failed(message) => (TaskStatus::Failed, Some(message)),
                TaskOutcome::Canceled => (TaskStatus::Canceled, None),
            };
            match status {
                TaskStatus::Completed => counts.completed += 1,
                TaskStatus::Failed => counts.failed += 1,
                _ => canceled = true,
            }
            self.staging.write_task(
                scanner,
                task,
                &task_attrs(status, Some(task_started), Some(now_ms())),
            )?;
            if let Some(message) = &error {
                logs.err(message)?;
            }
            drop(logs);
            if let Some(message) = &error {
                self.say(
                    &mut scan_logs,
                    ScanOutput::Err(format!("task {scanner}:{task} failed\n{message}")),
                )?;
            }
            if status == TaskStatus::Canceled {
                self.say(&mut scan_logs, ScanOutput::Err("scan canceled\n".into()))?;
            }
            (self.on_event)(Event::TaskFinished {
                scanner: scanner.clone(),
                task: task.clone(),
                status,
                error,
            });
        }

        let attrs = ScanAttrs {
            runtime: format!("gage {}", self.config.gage_version),
            started,
            stopped: now_ms(),
            canceled,
            tasks: counts,
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
        let commit_sha = ScanStore::from(store).create(&self.id, &self.staging.scan_dir())?;
        self.staging.mark_applied()?;
        self.staging.remove()?;
        Ok(ScanOutcome {
            id: self.id,
            commit_sha,
            attrs,
        })
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

/// The scan's closing line: `scan <id> completed: 3 tasks: 2
/// completed, 1 failed`, with a canceled count when the run was cut
/// short.
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
    let canceled = counts.total - counts.completed - counts.failed - counts.skipped;
    if canceled > 0 {
        parts.push(format!("{canceled} canceled"));
    }
    format!(
        "scan {id} {state}: {} tasks: {}",
        counts.total,
        parts.join(", ")
    )
}

/// The `(scanner, task)` pairs to run, in scanner order then task
/// order. Scanner names must be unique.
fn plan_tasks(scanners: &[CompiledScanner]) -> Result<Vec<(String, String)>, ScanError> {
    let mut plan = Vec::new();
    for (i, scanner) in scanners.iter().enumerate() {
        if scanners.iter().take(i).any(|s| s.name == scanner.name) {
            return Err(ScanError::DuplicateScanner(scanner.name.clone()));
        }
        for task in &scanner.tasks {
            plan.push((scanner.name.clone(), task.clone()));
        }
    }
    Ok(plan)
}

fn task_attrs(status: TaskStatus, started: Option<i64>, stopped: Option<i64>) -> TaskAttrs {
    TaskAttrs {
        status,
        started,
        stopped,
        worked_ms: None,
    }
}

enum TaskOutcome {
    Completed,
    /// The rendered failure message
    Failed(String),
    Canceled,
}

/// Run one task on a fresh VM, writing its output to `logs` and
/// forwarding it to `on_event` as it happens.
async fn run_task(
    scanner: &CompiledScanner,
    task: &str,
    cancel: &CancellationToken,
    logs: &mut Logs,
    on_event: &mut impl FnMut(Event),
) -> Result<TaskOutcome, ScanError> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = {
        let exec = OUTPUT_TX.scope(tx, execute(scanner, task));
        tokio::pin!(exec);
        loop {
            tokio::select! {
                result = &mut exec => break match result {
                    Ok(()) => TaskOutcome::Completed,
                    Err(message) => TaskOutcome::Failed(message),
                },
                Some(output) = rx.recv() => deliver(output, logs, on_event)?,
                _ = cancel.cancelled() => break TaskOutcome::Canceled,
            }
        }
    };
    // The block dropped the execution and with it the sender; drain
    // what the task sent between the last poll and completion.
    while let Ok(output) = rx.try_recv() {
        deliver(output, logs, on_event)?;
    }
    Ok(outcome)
}

/// Record one output in the task's logs, then hand it to the sink.
fn deliver(
    output: Output,
    logs: &mut Logs,
    on_event: &mut impl FnMut(Event),
) -> Result<(), ScanError> {
    match &output {
        Output::Print(s) => logs.out(s)?,
        Output::Println(s) => {
            logs.out(s)?;
            logs.out("\n")?;
        }
        Output::Log { level, message } => logs.record(*level, message)?,
    }
    on_event(Event::Output(output));
    Ok(())
}

async fn execute(scanner: &CompiledScanner, task: &str) -> Result<(), String> {
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
fn task_result(value: Value, scanner: &CompiledScanner, task: &str) -> Result<(), String> {
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
fn returned_error(value: &str, scanner: &CompiledScanner, task: &str) -> String {
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
    use gage_registry::scanner::parse_scanner_file;
    use tempfile::TempDir;

    use super::*;

    /// Write `source` as a scanner file and compile it. The directory
    /// guard is returned so the file outlives the compiled scanner.
    fn compile_source(source: &str) -> (TempDir, Result<CompiledScanner, Error>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scanner.rn");
        std::fs::write(&path, source).unwrap();
        let def = parse_scanner_file(&path).unwrap();
        let compiled = compile(&def);
        (dir, compiled)
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
        };
        let outcome = scan(store, &config, scanners, cancel, |e| events.push(e)).await;
        (outcome, events)
    }

    fn outputs(events: &[Event]) -> Vec<&Output> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Output(o) => Some(o),
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
                Event::Output(Output::Println("b ran".into())),
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
                "scan {} completed: 2 tasks: 1 completed, 1 failed\n",
                outcome.id
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
            Some(summary.into_bytes()),
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
        assert_eq!(a.logs, ["err"]);
        assert_eq!(
            ScanStore::from(&store)
                .task_log(&outcome.commit_sha, "fail", "a", "err")
                .unwrap(),
            Some(failure.into_bytes()),
            "err holds the full diagnostic"
        );
        assert!(a.attrs.started.is_some() && a.attrs.stopped.is_some());
        assert_eq!(b.task, "b");
        assert_eq!(b.attrs.status, TaskStatus::Completed);
        assert_eq!(b.logs, ["out"]);

        assert!(
            !root.join(&outcome.id).exists(),
            "staging is removed after apply"
        );
    }

    /// Print output and log records land in the task's `logs/`, and a
    /// task that produced neither has no logs.
    #[tokio::test]
    async fn task_output_and_records_are_stored_under_logs() {
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
        assert!(events.contains(&Event::Output(Output::Log {
            level: gage_runtime2::Level::Warn,
            message: "careful".into(),
        })));
        let scans = ScanStore::from(&store);
        let record = scans.get(&outcome.id).unwrap();
        assert_eq!(record.content.tasks[0].task, "loud");
        assert_eq!(record.content.tasks[0].logs, ["out", "records"]);
        assert_eq!(record.content.tasks[1].task, "quiet");
        assert!(record.content.tasks[1].logs.is_empty());
        assert_eq!(
            scans
                .task_log(&outcome.commit_sha, "logs", "loud", "out")
                .unwrap(),
            Some(b"ab\n".to_vec())
        );
        let records = scans
            .task_log(&outcome.commit_sha, "logs", "loud", "records")
            .unwrap()
            .unwrap();
        let records = String::from_utf8(records).unwrap();
        let lines: Vec<&str> = records.lines().collect();
        assert_eq!(lines.len(), 2, "{records}");
        assert!(lines[0].ends_with("Z INFO count 3"), "{records}");
        assert!(lines[1].ends_with("Z WARN careful"), "{records}");
        assert_eq!(
            scans
                .task_log(&outcome.commit_sha, "logs", "quiet", "out")
                .unwrap(),
            None
        );
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
        let compiled = compile(&parse_scanner_file(&path).unwrap()).unwrap();
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
                "scan {} canceled: 2 tasks: 0 completed, 0 failed, 2 canceled\n",
                outcome.id
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

    /// Runtime `tracing` events go to the scan's `records` outside a
    /// task and to the task's `records` inside one, each with the
    /// Rust target after the level.
    #[tokio::test]
    async fn runtime_records_route_to_the_scan_or_the_running_task() {
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

        let (_dir, compiled) = compile_source(HELLO);
        let (tmp, store) = open_store();
        let config = ScanConfig {
            staging_root: &tmp.path().join("staging"),
            gage_version: "test-version",
        };
        let outcome = scan(
            &store,
            &config,
            &[compiled.unwrap()],
            &CancellationToken::new(),
            |event| match event {
                // Emitted from the scan loop, outside any task
                Event::TaskStarted { .. } => tracing::warn!("outside the task"),
                // Delivered from inside the running task's scope
                Event::Output(Output::Print(_)) => tracing::warn!("inside the task"),
                _ => {}
            },
        )
        .await
        .unwrap();

        let scans = ScanStore::from(&store);
        let scan_records = String::from_utf8(
            scans
                .scan_log(&outcome.commit_sha, "records")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let task_records = String::from_utf8(
            scans
                .task_log(&outcome.commit_sha, "hello", "hello", "records")
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            scan_records.contains(" INFO gage_scan2: scan ")
                && scan_records.contains(" started with 1 tasks\n"),
            "{scan_records}"
        );
        assert!(
            scan_records.contains(" WARN gage_scan2::tests: outside the task\n"),
            "{scan_records}"
        );
        assert!(!scan_records.contains("inside the task"), "{scan_records}");
        assert!(
            task_records.contains(" WARN gage_scan2::tests: inside the task\n"),
            "{task_records}"
        );
        assert!(!task_records.contains("outside the task"), "{task_records}");
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
}
