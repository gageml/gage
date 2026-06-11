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

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use rune::Options;
use rune::languageserver;

#[tokio::main]
async fn main() -> Result<()> {
    let context = gage_scan::lsp_context(scanners_dir())?;
    let options = Options::from_default_env()?;

    let server = languageserver::builder()
        .with_context(context)
        .with_options(options)
        .with_stdio()
        .build()?;

    server.run().await?;
    Ok(())
}

/// The scanners directory whose layout the `include_*` macros resolve against.
///
/// Editors launch the server with the working directory set to the project
/// root (the nvim config roots on `.git`), and scanners live under
/// `<root>/scanners`. An explicit path may be passed as the first argument to
/// override this.
fn scanners_dir() -> PathBuf {
    if let Some(arg) = env::args().nth(1) {
        return PathBuf::from(arg);
    }
    match env::current_dir() {
        Ok(cwd) => cwd.join("scanners"),
        Err(_) => PathBuf::from("scanners"),
    }
}
