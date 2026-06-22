//! Rune language server for Gage scanners.
//!
//! The stock `rune-languageserver` compiles scanner sources against Rune's
//! default context, so every Gage native symbol (`gage::user()`, `io`,
//! `stats`, `json`, the `include_*` macros) reports as a missing item. This
//! binary runs the same Rune language server with Gage's context instead, so
//! diagnostics, go-to-definition, and completion match what scanners actually
//! run against (`gage_scan::lsp_context`).
//!
//! It is an internal dev tool (`dist = false`), built and put on `PATH` for
//! editor integration — it is not part of the shipped `gage` binary.

use anyhow::Result;
use rune::Options;
use rune::languageserver;

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = gage_log::init("lsp")?;
    let context = gage_scan::lsp_context()?;
    let options = Options::from_default_env()?;

    let server = languageserver::builder()
        .with_context(context)
        .with_options(options)
        .with_stdio()
        .build()?;

    server.run().await?;
    Ok(())
}
