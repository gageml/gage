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

pub mod staging;

use std::fmt;
use std::io;
use std::sync::Arc;

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_registry::scanner::ScannerDef;
use gage_runtime2::source::{SourceError, SourceFile, source_files};
use gage_runtime2::{OUTPUT_TX, Output};
use gage_scan::error::render_task_error;
use gage_scan::runner::render_vm_error;
use gage_store::{ScanAttrs, ScanStore, Store, StoreError, TaskAttrs, TaskCounts, TaskStatus};
use rune::runtime::{RuntimeContext, Unit, Value, VmError};
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources, Vm};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::staging::{ScannerPlan, Staging, State};

/// One item of run output, in the order it happened.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Task output
    Output(Output),
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
    mut on_event: impl FnMut(Event),
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

    let started = now_ms();
    let mut counts = TaskCounts {
        total: plan.len(),
        ..TaskCounts::default()
    };
    let mut canceled = false;
    for (scanner, task) in &plan {
        if cancel.is_cancelled() {
            canceled = true;
            staging.write_task(scanner, task, &task_attrs(TaskStatus::Canceled, None, None))?;
            on_event(Event::TaskFinished {
                scanner: scanner.clone(),
                task: task.clone(),
                status: TaskStatus::Canceled,
                error: None,
            });
            continue;
        }
        let compiled = scanners
            .iter()
            .find(|s| &s.name == scanner)
            .expect("plan names a compiled scanner");
        let task_started = now_ms();
        staging.write_task(
            scanner,
            task,
            &task_attrs(TaskStatus::Started, Some(task_started), None),
        )?;
        on_event(Event::TaskStarted {
            scanner: scanner.clone(),
            task: task.clone(),
        });
        let outcome = run_task(compiled, task, cancel, &mut on_event).await;
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
        staging.write_task(
            scanner,
            task,
            &task_attrs(status, Some(task_started), Some(now_ms())),
        )?;
        if let Some(message) = &error {
            staging.write_task_error(scanner, task, message)?;
        }
        on_event(Event::TaskFinished {
            scanner: scanner.clone(),
            task: task.clone(),
            status,
            error,
        });
    }

    let attrs = ScanAttrs {
        runtime: format!("gage {}", config.gage_version),
        started,
        stopped: now_ms(),
        canceled,
        tasks: counts,
    };
    staging.write_scan(&attrs)?;
    staging.set_state(if canceled {
        State::Canceled
    } else {
        State::Completed
    })?;
    let commit_sha = ScanStore::from(store).create(&id, &staging.scan_dir())?;
    staging.mark_applied()?;
    staging.remove()?;
    Ok(ScanOutcome {
        id,
        commit_sha,
        attrs,
    })
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

/// Run one task on a fresh VM, forwarding its output while it runs.
async fn run_task(
    scanner: &CompiledScanner,
    task: &str,
    cancel: &CancellationToken,
    on_event: &mut impl FnMut(Event),
) -> TaskOutcome {
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
                Some(output) = rx.recv() => on_event(Event::Output(output)),
                _ = cancel.cancelled() => break TaskOutcome::Canceled,
            }
        }
    };
    // The block dropped the execution and with it the sender; drain
    // what the task sent between the last poll and completion.
    while let Ok(output) = rx.try_recv() {
        on_event(Event::Output(output));
    }
    outcome
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
    task_result(value)
}

fn vm_error(e: &VmError, sources: &Sources) -> String {
    render_vm_error(e, sources, &e.to_string())
}

/// Interpret a task's return value. A task returning unit or `Ok`
/// succeeded; `Err(e)` fails with `e` rendered.
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the runtime holds the only live handle"
)]
fn task_result(value: Value) -> Result<(), String> {
    match rune::from_value::<Result<Value, Value>>(value) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(render_task_error(err)),
        // Not a Result: a task that returns unit or any other value
        Err(_) => Ok(()),
    }
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
    /// task, the failed task's message in `error.txt`, and staging
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
        assert_eq!(
            events,
            [
                Event::TaskStarted {
                    scanner: "fail".into(),
                    task: "a".into(),
                },
                Event::TaskFinished {
                    scanner: "fail".into(),
                    task: "a".into(),
                    status: TaskStatus::Failed,
                    error: Some("boom".into()),
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
            ]
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
        assert_eq!(a.error.as_deref(), Some("boom"));
        assert!(a.attrs.started.is_some() && a.attrs.stopped.is_some());
        assert_eq!(b.task, "b");
        assert_eq!(b.attrs.status, TaskStatus::Completed);
        assert_eq!(b.error, None);

        assert!(
            !root.join(&outcome.id).exists(),
            "staging is removed after apply"
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
        }) = events.last()
        else {
            panic!("expected a failure, got {events:?}");
        };
        assert!(message.contains("v[3]"), "{message}");
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
        assert!(events.iter().all(|e| matches!(
            e,
            Event::TaskFinished {
                status: TaskStatus::Canceled,
                ..
            }
        )));
        let record = ScanStore::from(&store).get(&outcome.id).unwrap();
        assert!(
            record
                .content
                .tasks
                .iter()
                .all(|t| { t.attrs.status == TaskStatus::Canceled && t.attrs.started.is_none() })
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
}
