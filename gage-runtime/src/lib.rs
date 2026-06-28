pub mod agent;
pub mod config;
pub mod datetime;
pub mod db;
pub mod error;
pub mod ignore;
pub mod io;
pub mod json;
pub mod llm;
pub mod macros;
pub mod query;
mod result;
pub mod scan;
pub mod state;
pub mod stats;
pub mod template;
pub mod value;

pub(crate) use result::Result;

use rune::{Context, ContextError, Module};

/// Output emitted by Rune-runtime `print`/`println` calls. The orchestrator
/// owns the receiver end of an `UnboundedSender<RuntimeOutput>` it places in
/// every task's [`state::ScanContext`].
#[derive(Debug)]
pub enum RuntimeOutput {
    Print(String),
    Println(String),
}

/// Build a Rune context for the language server: the same native modules the
/// runtime installs (`runner::run`), so scanner sources resolve `gage::*`,
/// `io`, `stats`, `json`, and the `include_*` macros.
pub fn lsp_context() -> std::result::Result<Context, ContextError> {
    let mut context = rune_modules::with_config(false)?;
    context.install(io_module()?)?;
    context.install(types_module()?)?;
    context.install(macros_module()?)?;
    context.install(gage_module()?)?;
    context.install(stats_module()?)?;
    context.install(json_module()?)?;
    Ok(context)
}

pub fn macros_module() -> std::result::Result<Module, ContextError> {
    macros::module()
}

pub fn gage_module() -> std::result::Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;

    scan::register(&mut m)?;
    config::register(&mut m)?;
    query::register(&mut m)?;
    db::register(&mut m)?;
    llm::register(&mut m)?;
    agent::register(&mut m)?;
    template::register(&mut m)?;

    Ok(m)
}

pub fn io_module() -> std::result::Result<Module, ContextError> {
    io::module()
}

pub fn stats_module() -> std::result::Result<Module, ContextError> {
    stats::module()
}

pub fn json_module() -> std::result::Result<Module, ContextError> {
    json::module()
}

pub fn types_module() -> std::result::Result<Module, ContextError> {
    scan::types_module()
}
