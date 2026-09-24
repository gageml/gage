//! Second-generation scanner runtime for the schema rethink, paired
//! with `gage-scan2`. The first-generation crates (`gage-runtime`,
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
//! - The scanner-facing surface is the exception: `scan()` and the
//!   values it returns (`Scan`, `Session`, `Sessions`) are what the
//!   rethink changes. A session is a store object read through its
//!   driver, not a file path, and the task context is the store and
//!   the dataset commit, not the sqlite db. Legacy `scan()` reads
//!   state this runtime never populates, so that surface is written
//!   here rather than called, and its Rune boilerplate (iterator
//!   protocols, getters) is not history worth preserving. Everything
//!   below that surface, such as content reading, query tables, agent
//!   calls, templates, validation, and the datetime, json, and stats
//!   modules, reaches this crate by calling into the first generation.
//!
//! This crate owns the Rune-facing half of the facility: the context
//! every scanner compiles against, the native modules installed in
//! it, the per-task state those modules read, and the rule for which
//! files are a scanner's source ([`source`]). Task orchestration lives
//! in `gage-scan2`.

mod io;
mod log;
mod query;
mod scan;
pub mod source;

use rune::{Context, ContextError};
use tokio::sync::mpsc;

pub use scan::{SCAN_CTX, Scan, ScanContext, ScanDataset, ScanDatasetRef, Session, Sessions};

/// One item of task output, in the order it happened. The runtime
/// emits these; the consumer owns rendering.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// A scanner `print(...)` call, verbatim
    Print(String),
    /// A scanner `println(...)` call, verbatim, with no trailing newline
    Println(String),
    /// A scanner `log::<level>!(...)` record
    Log { level: Level, message: String },
}

/// A log record's level, the set the Rust `log` crate defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const ALL: [Level; 5] = [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
    ];

    /// The lowercase name, the macro name in Rune
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// One item of task output with the task that produced it.
#[derive(Debug, PartialEq, Eq)]
pub struct TaskOutput {
    pub scanner: String,
    pub task: String,
    pub output: Output,
}

/// The running task's identity and the scan's output channel. The
/// task orchestrator scopes one per task execution; every task of a
/// scan shares the sender, and the orchestrator is the only receiver,
/// so receive order is the order of the scan.
#[derive(Debug, Clone)]
pub struct OutputSink {
    pub scanner: String,
    pub task: String,
    pub tx: mpsc::UnboundedSender<TaskOutput>,
}

tokio::task_local! {
    /// The running task's sink, read by the `std::io` replacement in
    /// [`io`] and the `log` macros in [`log`]
    pub static OUTPUT_SINK: OutputSink;
}

/// Send one output item from the running task.
pub(crate) fn send(output: Output) {
    OUTPUT_SINK.with(|sink| {
        sink.tx
            .send(TaskOutput {
                scanner: sink.scanner.clone(),
                task: sink.task.clone(),
                output,
            })
            .expect("output receiver should be held open for the task's lifetime")
    });
}

/// The Rune context every scanner compiles against: the standard
/// library without its stdio, this crate's `print`/`println` and
/// `log` macros, `gage::scan` and the values it returns, the message
/// and entry queries on a session, and the include macros from
/// `gage-runtime`. Every file-reading facility
/// installed here is enumerated by [`source::source_files`].
pub fn context() -> Result<Context, ContextError> {
    let mut context = Context::with_config(false)?;
    context.install(io::module()?)?;
    context.install(log::module()?)?;
    context.install(scan::module()?)?;
    context.install(scan::types_module()?)?;
    context.install(query::types_module()?)?;
    context.install(gage_runtime::macros_module()?)?;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources, Vm};
    use tokio::sync::mpsc;

    use super::*;

    /// Run `main` of `script` under an output channel and return what
    /// it sent.
    async fn outputs_of(script: &str) -> Vec<Output> {
        let context = context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        let unit = RuneArc::try_new(unit).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "s".into(),
            task: "main".into(),
            tx,
        };
        OUTPUT_SINK
            .scope(sink, async move {
                let vm = Vm::new(rt, unit);
                vm.send_execute(["main"], ())
                    .unwrap()
                    .complete()
                    .await
                    .unwrap();
            })
            .await;
        let mut out = Vec::new();
        while let Ok(o) = rx.try_recv() {
            assert_eq!((o.scanner.as_str(), o.task.as_str()), ("s", "main"));
            out.push(o.output);
        }
        out
    }

    #[tokio::test]
    async fn log_macros_send_leveled_records_in_order() {
        let outputs = outputs_of(
            r#"
            pub fn main() {
                log::info!("scanning {} sessions", 12);
                println!("progress");
                log::warn!("no project for {}", "3f02");
                log::trace!("t");
                log::debug!("d");
                log::error!("e");
            }
            "#,
        )
        .await;
        assert_eq!(
            outputs,
            [
                Output::Log {
                    level: Level::Info,
                    message: "scanning 12 sessions".into(),
                },
                Output::Println("progress".into()),
                Output::Log {
                    level: Level::Warn,
                    message: "no project for 3f02".into(),
                },
                Output::Log {
                    level: Level::Trace,
                    message: "t".into(),
                },
                Output::Log {
                    level: Level::Debug,
                    message: "d".into(),
                },
                Output::Log {
                    level: Level::Error,
                    message: "e".into(),
                },
            ]
        );
    }
}
