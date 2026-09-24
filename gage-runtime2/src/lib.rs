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
//!
//! This crate owns the Rune-facing half of the facility: the context
//! every scanner compiles against, the native modules installed in
//! it, the per-task state those modules read, and the rule for which
//! files are a scanner's source ([`source`]). Task orchestration lives
//! in `gage-scan2`.

mod io;
mod log;
pub mod source;

use rune::{Context, ContextError};
use tokio::sync::mpsc;

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

tokio::task_local! {
    /// Sender for the running task's output, read by the `std::io`
    /// replacement in [`io`]. The task orchestrator scopes one sender
    /// per task execution.
    pub static OUTPUT_TX: mpsc::UnboundedSender<Output>;
}

/// The Rune context every scanner compiles against: the standard
/// library without its stdio, this crate's `print`/`println` and
/// `log` macros, and the include macros from `gage-runtime`. Every
/// file-reading facility installed here is enumerated by
/// [`source::source_files`].
pub fn context() -> Result<Context, ContextError> {
    let mut context = Context::with_config(false)?;
    context.install(io::module()?)?;
    context.install(log::module()?)?;
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
        OUTPUT_TX
            .scope(tx, async move {
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
            out.push(o);
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
