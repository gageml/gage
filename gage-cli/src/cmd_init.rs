use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use cliclack as cli;
use gage_claude::plugin;
use gage_claude::proc::find_claude;
use gage_core::config::plugin_marketplace_dir;

use crate::dialog::{self, DialogError, DialogResult};

#[derive(Args)]
pub struct InitArgs {
    /// Remove Gage from Claude Code instead of installing
    #[arg(long)]
    pub remove: bool,

    /// Skip confirmation prompts
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(args: InitArgs) {
    if args.remove {
        dialog::run("Remove Gage setup", || remove_dialog(&args));
    } else {
        dialog::run("Setup Gage", || install_dialog(&args));
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
    find_claude().map_err(|e| {
        DialogError::Other(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("claude not found on PATH: {e}"),
        ))
    })
}

fn run_claude(message: &str, claude_bin: &Path, args: &[&str]) -> Result<(), DialogError> {
    let spinner = crate::style::spinner(message);
    let output = Command::new(claude_bin)
        .args(args)
        .stderr(std::process::Stdio::inherit())
        .output();
    spinner.finish_and_clear();
    let output = output.map_err(|e| {
        DialogError::Other(std::io::Error::other(format!("failed to run claude: {e}")))
    })?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(DialogError::Other(std::io::Error::other(format!(
            "claude {} failed: {stdout}",
            args.join(" ")
        ))));
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
