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
}

tokio::task_local! {
    /// Sender for the running task's output, read by the `std::io`
    /// replacement in [`io`]. The task orchestrator scopes one sender
    /// per task execution.
    pub static OUTPUT_TX: mpsc::UnboundedSender<Output>;
}

/// The Rune context every scanner compiles against: the standard
/// library without its stdio, this crate's `print`/`println`, and
/// the include macros from `gage-runtime`. Every file-reading
/// facility installed here is enumerated by [`source::source_files`].
pub fn context() -> Result<Context, ContextError> {
    let mut context = Context::with_config(false)?;
    context.install(io::module()?)?;
    context.install(gage_runtime::macros_module()?)?;
    Ok(context)
}
