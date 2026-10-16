//! The scanner runtime: the Rune context every scanner compiles
//! against, the native modules installed in it, the per-task state
//! those modules read, and the rule for which files are a scanner's
//! source ([`source`]). Task orchestration lives in `gage-scan`.
//!
//! The scanner-facing surface is built on `gage-store`,
//! `gage-session`, and `gage-query`: a session is a store object
//! read through its driver, and the task context is the store and
//! the dataset commit.

mod agent;
mod attach;
mod attachment;
mod compress;
pub mod datetime;
pub mod error;
mod fail;
mod filters;
mod ignore;
mod io;
mod issue;
mod json;
mod key;
mod log;
mod macros;
mod note;
mod progress;
mod query;
pub mod row;
mod scan;
mod ser;
pub mod source;
mod stats;
mod target;
mod template;
mod tool;
mod tool_meta;
mod validate;
pub mod value;
mod view;

use rune::{Context, ContextError};
use tokio::sync::mpsc;

pub use attach::{ATTACH_CTX, AttachContext, Attached};
pub use attachment::{Attachment, AttachmentFile, Attachments};
pub use fail::Fail;
pub use ignore::is_ignore;
pub use log::LOG_TARGET;
pub use scan::{
    Native, SCAN_CTX, Scan, ScanContext, ScanDataset, ScanDatasetRef, Session, Sessions,
    render_vm_error,
};

/// The scheme term a scan records in its `runtime` attr, followed by
/// the version: `gage <version>`. The term names the runtime scheme
/// the scan ran under, not the application; a scan run by another
/// implementation of the same scheme records the same term.
pub const CURRENT_RUNTIME_SCHEME: &str = "gage";

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
    /// A `Progress` snapshot: absolute position and total
    Progress { pos: u64, total: u64 },
    /// One of the task's agents moved through the scan's agent pool
    AgentPool(AgentPoolDelta),
}

/// One agent's step through the scan's agent pool, reported by the
/// task that called it. A call that finds a permit free reports
/// `Acquired` alone; a call that has to wait reports `Queued` first.
/// Every `Acquired` is followed by one `Released`, so a consumer can
/// count the task's agents waiting for a permit and holding one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPoolDelta {
    /// `call_agent` is waiting for a permit
    Queued,
    /// The permit is held; the agent starts
    Acquired,
    /// The agent has ended and its permit is returned
    Released,
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
    OUTPUT_SINK.with(|sink| sink.send(output));
}

impl OutputSink {
    /// Send one output item under this sink's task
    pub(crate) fn send(&self, output: Output) {
        self.tx
            .send(TaskOutput {
                scanner: self.scanner.clone(),
                task: self.task.clone(),
                output,
            })
            .expect("output receiver should be held open for the task's lifetime")
    }
}

/// The Rune context every scanner compiles against: the standard
/// library and the `time` and `rand` modules without their stdio,
/// this crate's `print`/`println` and `log` macros, `gage::scan` and the values it returns, `gage::params`, the message
/// and entry queries on a session, `scan().attachments()` and the
/// values it returns, `gage::dataset`, `gage::attach`, `gage::Files`
/// and the values they return, `gage::Target`, `gage::write_note`,
/// `gage::write_issue`, `scan().issues()`, `gage::carry_forward_notes`,
/// `gage::carry_forward_notes_named`,
/// `gage::Mark`, `gage::watermark`, `gage::Template`, `gage::Progress`,
/// `gage::call_agent` and the values it returns, `gage::AgentRunner`,
/// `gage::Fail`, `gage::Ignore`, `session.compress(..)`, and the
/// `stats` and `json` modules and the include macros from
/// `gage-runtime`. Every file-reading facility installed here is
/// enumerated by [`source::source_files`].
pub fn context() -> Result<Context, ContextError> {
    let mut context = rune_modules::with_config(false)?;
    context.install(io::module()?)?;
    context.install(log::module()?)?;
    context.install(scan::module()?)?;
    context.install(scan::types_module()?)?;
    context.install(attachment::types_module()?)?;
    context.install(attach::module()?)?;
    context.install(attach::types_module()?)?;
    context.install(fail::types_module()?)?;
    context.install(query::types_module()?)?;
    context.install(compress::types_module()?)?;
    context.install(target::types_module()?)?;
    context.install(note::module()?)?;
    context.install(note::types_module()?)?;
    context.install(issue::module()?)?;
    context.install(issue::types_module()?)?;
    context.install(validate::module()?)?;
    context.install(validate::types_module()?)?;
    context.install(template::module()?)?;
    context.install(progress::types_module()?)?;
    context.install(agent::module()?)?;
    context.install(tool::module()?)?;
    context.install(tool::tools_module()?)?;
    context.install(stats::module()?)?;
    context.install(json::module()?)?;
    context.install(macros::module()?)?;
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
