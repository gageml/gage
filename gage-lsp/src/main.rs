//! Rune language server for Gage scanners.
//!
//! The stock `rune-languageserver` compiles scanner sources against Rune's
//! default context, so every Gage native symbol (`gage::scan()`,
//! `gage::write_note`, the `include_*` macros) reports as a missing item.
//! This binary runs the same Rune language server with Gage's context
//! instead, so diagnostics, go-to-definition, and completion match what
//! scanners actually run against.
//!
//! Two runtimes exist while the schema rethink is built: `--runtime 2`
//! (the default) serves the context `gage scan2` compiles against, and
//! `--runtime legacy` serves the first generation's (`gage scan`).
//!
//! The VS Code Rune extension probes the binary with `--version` and
//! then launches it as `gage-lsp language-server`, the stock server's
//! subcommand; both forms are accepted.
//!
//! It is an internal dev tool (`dist = false`), built and put on `PATH` for
//! editor integration — it is not part of the shipped `gage` binary.

use anyhow::{Result, bail};
use rune::{Context, Options, languageserver};

#[tokio::main]
async fn main() -> Result<()> {
    // The VS Code extension probes the binary with `--version` before
    // starting it
    if std::env::args().skip(1).any(|a| a == "--version") {
        println!("gage-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let _log_guard = gage_log::init("lsp")?;
    let context = context_for(std::env::args().skip(1))?;
    let options = Options::from_default_env()?;

    let server = languageserver::builder()
        .with_context(context)
        .with_options(options)
        .with_stdio()
        .build()?;

    server.run().await?;
    Ok(())
}

/// The context the arguments select: `--runtime 2` or `--runtime legacy`,
/// defaulting to `2`. The stock server's `language-server` subcommand
/// is accepted and ignored. Any other argument is an error.
fn context_for(args: impl Iterator<Item = String>) -> Result<Context> {
    let mut runtime = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "language-server" => {}
            "--runtime" => match args.next() {
                Some(value) => runtime = Some(value),
                None => bail!("--runtime requires a value: 2 or legacy"),
            },
            other => bail!(
                "unexpected argument {other:?}; \
                 usage: gage-lsp [language-server] [--runtime 2|legacy]"
            ),
        }
    }
    match runtime.as_deref().unwrap_or("2") {
        "2" => Ok(gage_runtime2::context()?),
        "legacy" => Ok(gage_scan::lsp_context()?),
        other => bail!("unknown runtime {other:?}: expected 2 or legacy"),
    }
}
