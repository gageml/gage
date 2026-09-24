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
//! the `gage-runtime2` context and runs their tasks. The runtime is a
//! pure event emitter: [`run`] hands each [`Event`] to the caller's
//! sink, which owns rendering.

use std::fmt;
use std::sync::Arc;

use gage_registry::scanner::ScannerDef;
use gage_runtime2::{OUTPUT_TX, Output};
use gage_scan::error::render_task_error;
use gage_scan::runner::render_vm_error;
use rune::runtime::{RuntimeContext, Unit, Value, VmError};
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources, Vm};
use tokio::sync::mpsc;

/// One item of run output, in the order it happened.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Task output
    Output(Output),
    /// A task returned an `Err` or the VM raised an error
    TaskFailed {
        scanner: String,
        task: String,
        message: String,
    },
}

#[derive(Debug)]
pub enum Error {
    Compile { name: String, diagnostics: String },
    MissingTask { scanner: String, task: String },
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
        rt,
        unit,
        sources: Arc::new(sources),
    })
}

/// End-of-run task accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
}

/// Run every task of every scanner, in order, one at a time. Output
/// reaches `on_output` as it happens; a failed task is reported
/// through the same sink and the run continues with the next task.
pub async fn run(scanners: &[CompiledScanner], mut on_event: impl FnMut(Event)) -> RunSummary {
    let mut summary = RunSummary::default();
    for scanner in scanners {
        for task in &scanner.tasks {
            summary.total += 1;
            match run_task(scanner, task, &mut on_event).await {
                Ok(()) => summary.completed += 1,
                Err(message) => {
                    summary.failed += 1;
                    on_event(Event::TaskFailed {
                        scanner: scanner.name.clone(),
                        task: task.clone(),
                        message,
                    });
                }
            }
        }
    }
    summary
}

/// Run one task on a fresh VM, forwarding its output while it runs.
/// The error is the rendered failure message.
async fn run_task(
    scanner: &CompiledScanner,
    task: &str,
    on_event: &mut impl FnMut(Event),
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let exec = OUTPUT_TX.scope(tx, execute(scanner, task));
    tokio::pin!(exec);
    let outcome = loop {
        tokio::select! {
            outcome = &mut exec => break outcome,
            Some(output) = rx.recv() => on_event(Event::Output(output)),
        }
    };
    // The scope dropped the sender when the task finished; drain what
    // it sent between the last poll and completion.
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

    use super::*;

    /// Write `source` as a scanner file and compile it. The directory
    /// guard is returned so the file outlives the compiled scanner.
    fn compile_source(source: &str) -> (tempfile::TempDir, Result<CompiledScanner, Error>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scanner.rn");
        std::fs::write(&path, source).unwrap();
        let def = parse_scanner_file(&path).unwrap();
        let compiled = compile(&def);
        (dir, compiled)
    }

    async fn collect(scanner: CompiledScanner) -> (RunSummary, Vec<Event>) {
        let mut events = Vec::new();
        let summary = run(&[scanner], |e| events.push(e)).await;
        (summary, events)
    }

    #[tokio::test]
    async fn print_and_println_reach_the_sink_in_order() {
        let (_dir, compiled) = compile_source(
            r#"
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
            "#,
        );
        let (summary, events) = collect(compiled.unwrap()).await;
        assert_eq!(
            events,
            [
                Event::Output(Output::Print("a".into())),
                Event::Output(Output::Println("b 2".into())),
                Event::Output(Output::Print("c".into())),
            ]
        );
        assert_eq!(
            summary,
            RunSummary {
                total: 1,
                completed: 1,
                failed: 0
            }
        );
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
        let (summary, events) = collect(compiled.unwrap()).await;
        assert_eq!(events, [Event::Output(Output::Println("go".into()))]);
        assert_eq!(summary.completed, 1);
    }

    #[tokio::test]
    async fn failing_task_is_reported_and_the_run_continues() {
        let (_dir, compiled) = compile_source(
            r#"
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
            "#,
        );
        let (summary, events) = collect(compiled.unwrap()).await;
        assert_eq!(
            events,
            [
                Event::TaskFailed {
                    scanner: "fail".into(),
                    task: "a".into(),
                    message: "boom".into(),
                },
                Event::Output(Output::Println("b ran".into())),
            ]
        );
        assert_eq!(
            summary,
            RunSummary {
                total: 2,
                completed: 1,
                failed: 1
            }
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
        let (summary, events) = collect(compiled.unwrap()).await;
        assert_eq!(summary.failed, 1);
        let [Event::TaskFailed { message, .. }] = events.as_slice() else {
            panic!("expected one failure, got {events:?}");
        };
        assert!(message.contains("v[3]"), "{message}");
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
