//! `log`: scanner logging, mirroring the Rust `log` crate.
//!
//! `log::trace!` through `log::error!` accept the full format grammar
//! and expand to `::log::__write(level, message)`, which sends one
//! [`Output::Log`] record through the task's output sink and raises
//! the same record as a `tracing` event under [`LOG_TARGET`] with
//! `scanner` and `task` fields. The scanner code's counterpart of a
//! module path is `scanner::<scanner>::<task>`, but a `tracing` event
//! carries `'static` metadata, so the two names travel as fields and
//! a subscriber that renders the path assembles it from them. The
//! scan writes the sink's record to its `logs/records`; the event is
//! for live subscribers such as the CLI's stderr layer, so a
//! subscriber that also writes `logs/records` skips the target.
//! Distinct from `println`, which is task output and lands in
//! `logs/out`.
//!
//! The module lives at the root (`::log`), not under `::gage`: Rune
//! resolves macro paths literally, so `log::info!(...)` only works if
//! `::log::info` is the macro's registered path.

use rune::macros::FormatArgs;
use rune::parse::Parser;
use rune::{ContextError, Module};

use crate::{Level, OUTPUT_SINK, Output, send};

/// The `tracing` target of the events the `log` macros raise. Their
/// `scanner` and `task` fields name the running task.
pub const LOG_TARGET: &str = "scanner";

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("log")?;
    for level in Level::ALL {
        let name = level.as_str();
        m.macro_([name], move |cx, stream| {
            let mut p = Parser::from_token_stream(stream, cx.input_span());
            let args = p.parse_all::<FormatArgs>()?;
            let expanded = args.expand(cx)?;
            let lit = cx.lit(name)?;
            Ok(rune::macros::quote!(::log::__write(#lit, #expanded)).into_token_stream(cx)?)
        })?;
    }
    m.function("__write", __write).build()?;
    Ok(m)
}

/// Expansion target for the level macros. A level outside the set can
/// only come from a direct call, not from the macros, and is recorded
/// as an error.
fn __write(level: &str, message: &str) {
    let level = Level::ALL
        .into_iter()
        .find(|l| l.as_str() == level)
        .unwrap_or(Level::Error);
    send(Output::Log {
        level,
        message: message.to_string(),
    });
    OUTPUT_SINK.with(|sink| {
        let (scanner, task) = (&sink.scanner, &sink.task);
        match level {
            Level::Trace => tracing::trace!(target: LOG_TARGET, %scanner, %task, "{message}"),
            Level::Debug => tracing::debug!(target: LOG_TARGET, %scanner, %task, "{message}"),
            Level::Info => tracing::info!(target: LOG_TARGET, %scanner, %task, "{message}"),
            Level::Warn => tracing::warn!(target: LOG_TARGET, %scanner, %task, "{message}"),
            Level::Error => tracing::error!(target: LOG_TARGET, %scanner, %task, "{message}"),
        }
    });
}
