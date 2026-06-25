use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use cliclack as cli;
use gage_claude::plugin;
use gage_claude::proc::find_claude;
use gage_core::config::plugin_marketplace_dir;
use gage_db::import::{ImportReport, import};

use crate::dialog::{self, DialogError, DialogResult};

#[derive(Args)]
pub struct InitArgs {
    /// Uninstall Gage from Claude Code
    #[arg(short, long, conflicts_with_all = ["import_data", "import_data_preview"])]
    pub remove: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// Import data from PATH
    ///
    /// Rows with same IDs are never overwritten. Rejected rows are written to a
    /// JSON file next to the modified database file (~/.gage/data/gage.db)
    #[arg(long, value_name = "PATH", conflicts_with = "import_data_preview")]
    pub import_data: Option<PathBuf>,

    /// Preview import without modifying the database
    #[arg(long, value_name = "PATH")]
    pub import_data_preview: Option<PathBuf>,
}

pub fn run(args: InitArgs) {
    if let Some(p) = args
        .import_data
        .as_ref()
        .or(args.import_data_preview.as_ref())
    {
        let preview = args.import_data_preview.is_some();
        match import(p, preview) {
            Ok(report) => print_import_report(&report),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.remove {
        dialog::run("Remove Gage setup", || remove_dialog(&args));
    } else {
        dialog::run("Setup Gage", || install_dialog(&args));
    }
}

fn print_import_report(r: &ImportReport) {
    let mode = if r.preview { " (preview)" } else { "" };
    println!("Import from {}{mode}", r.source.display());
    println!("  {:<18}  {:>10}  {:>10}", "table", "accepted", "rejected");
    for t in &r.tables {
        println!("  {:<18}  {:>10}  {:>10}", t.name, t.accepted, t.rejected);
    }
    if let Some(p) = &r.rejected_path {
        println!("Rejected rows written to {}", p.display());
    } else if !r.preview && r.tables.iter().all(|t| t.rejected == 0) {
        println!("No rejected rows");
    }
}

fn install_dialog(args: &InitArgs) -> Result<DialogResult, DialogError> {
    let claude_bin = find_claude_or_err()?;
    let marketplace = plugin_marketplace_dir();

    cli::log::step("Plugin\ngage (MCP server + skills)")?;

    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let gage_bin = std::env::current_exe()?;
    plugin::write_plugin_files_to(&marketplace, &gage_bin)?;
    plugin::write_marketplace_manifest_to(&marketplace)?;

    run_claude(
        "Registering plugin marketplace",
        &claude_bin,
        &[
            "plugin",
            "marketplace",
            "add",
            &marketplace.to_string_lossy(),
        ],
    )?;
    run_claude(
        "Installing plugin",
        &claude_bin,
        &["plugin", "install", "gage@gage"],
    )?;

    Ok(DialogResult::from("Gage installed as Claude Code plugin"))
}

fn remove_dialog(args: &InitArgs) -> Result<DialogResult, DialogError> {
    let claude_bin = find_claude_or_err()?;

    cli::log::step("Plugin\ngage@gage")?;

    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    run_claude_best_effort(
        "Uninstalling plugin",
        &claude_bin,
        &["plugin", "uninstall", "gage@gage"],
    )?;
    run_claude_best_effort(
        "Removing marketplace",
        &claude_bin,
        &["plugin", "marketplace", "remove", "gage"],
    )?;

    Ok(DialogResult::from("Gage removed from Claude Code"))
}

fn find_claude_or_err() -> Result<PathBuf, DialogError> {
    find_claude().map_err(|e| DialogError::Other(anyhow::anyhow!("claude not found on PATH: {e}")))
}

fn run_claude(message: &str, claude_bin: &Path, args: &[&str]) -> Result<(), DialogError> {
    let spinner = crate::style::spinner(message);
    let output = Command::new(claude_bin)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output();
    spinner.finish_and_clear();
    let output =
        output.map_err(|e| DialogError::Other(anyhow::anyhow!("failed to run claude: {e}")))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(DialogError::Other(anyhow::anyhow!(
            "claude {} failed: {stdout}",
            args.join(" ")
        )));
    }
    Ok(())
}

fn run_claude_best_effort(
    message: &str,
    claude_bin: &Path,
    args: &[&str],
) -> Result<(), DialogError> {
    let spinner = crate::style::spinner(message);
    let output = Command::new(claude_bin)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output();
    spinner.finish_and_clear();
    match output {
        Ok(o) if !o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            cli::log::warning(format!("claude {} failed: {stdout}", args.join(" ")))?;
        }
        Ok(_) => {}
        Err(e) => {
            cli::log::warning(format!("failed to run claude {}: {e}", args.join(" ")))?;
        }
    }
    Ok(())
}
