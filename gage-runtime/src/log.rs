//! `log` — scanner logging into the runtime's tracing output.
//!
//! Mirrors the tracing macro set: `log::trace!` through `log::error!`,
//! each accepting the full format grammar (`log::info!("{foo:?}")`).
//! Every macro expands to a call to the single hook `__write(level,
//! message)`, which emits one tracing event tagged with the calling
//! scanner's name — scanner log lines land in the same per-process log
//! files as the rest of the runtime. Distinct from `println`, which is
//! user-facing scan progress output.
//!
//! The module lives at the root (`::log`), not under `::gage`: Rune
//! resolves macro paths literally, without following `use` imports, so
//! `log::info!(...)` only works if `::log::info` is the macro's actual
//! registered path. No import is needed at the call site.

use rune::macros::FormatArgs;
use rune::parse::Parser;
use rune::{ContextError, Module};

use crate::state::current_scan_ctx;

const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("log")?;
    for level in LEVELS {
        m.macro_([level], move |cx, stream| {
            let mut p = Parser::from_token_stream(stream, cx.input_span());
            let args = p.parse_all::<FormatArgs>()?;
            let expanded = args.expand(cx)?;
            let lit = cx.lit(*level)?;
            Ok(rune::macros::quote!(::log::__write(#lit, #expanded)).into_token_stream(cx)?)
        })?;
    }
    m.function("__write", __write).build()?;
    Ok(m)
}

/// Expansion target for the level macros. `level` is one of
/// [`LEVELS`]; anything else logs at error level with a spoof marker,
/// which can only happen when called directly rather than through the
/// macros.
fn __write(level: &str, msg: &str) {
    let scanner = &current_scan_ctx().scanner_name;
    match level {
        "trace" => tracing::trace!(scanner, "{msg}"),
        "debug" => tracing::debug!(scanner, "{msg}"),
        "info" => tracing::info!(scanner, "{msg}"),
        "warn" => tracing::warn!(scanner, "{msg}"),
        "error" => tracing::error!(scanner, "{msg}"),
        other => tracing::error!(scanner, level = other, "{msg}"),
    }
}
