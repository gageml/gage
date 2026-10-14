use clap::Args;
use cliclack as cli;
use gage_session::InstallUi;
use gage_store::{Store, StoreError};

use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::source::driver_registry;

#[derive(Args)]
pub struct InitArgs {
    /// Remove Gage from Claude Code
    #[arg(short, long)]
    pub remove: bool,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(args: InitArgs) -> Result<(), CliError> {
    if args.remove {
        dialog::run("Remove Gage setup", || remove_dialog(&args))
    } else {
        dialog::run("Setup Gage", || install_dialog(&args))
    }
}

fn install_dialog(args: &InitArgs) -> Result<DialogResult, DialogError> {
    let registry = driver_registry();
    let driver = registry
        .default()
        .ok_or_else(|| DialogError::Other(anyhow::anyhow!("no default driver registered")))?;

    cli::log::step("Gage store")?;
    cli::log::step(format!(
        "Plugin\ngage (MCP server + skills) via {} driver",
        driver.name()
    ))?;

    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let spinner = cli::spinner();
    spinner.start("Initializing store");
    match init_store() {
        Ok(done) => spinner.stop(done),
        Err(e) => {
            spinner.clear();
            return Err(e);
        }
    }

    let gage_bin = std::env::current_exe()?;
    let gage_bin_str = gage_bin.to_str().ok_or_else(|| {
        DialogError::Other(anyhow::anyhow!(
            "gage binary path is not valid UTF-8: {}",
            gage_bin.display()
        ))
    })?;

    let mut ui = SpinnerUi::default();
    driver
        .system_init(&[gage_bin_str, "mcp"], &mut ui)
        .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
    ui.finish();

    Ok(DialogResult::from("Initialized Gage"))
}

/// Creates the store only when none exists. An existing store is
/// opened, which validates its version and object format without
/// running any git command that modifies it; `gage_store::init` on an
/// existing store would rewrite `gage.version` and the update hook.
/// Returns the line that records which happened.
fn init_store() -> Result<&'static str, DialogError> {
    let path = gage_store::store_path();
    let init_err = |e: StoreError| {
        DialogError::Other(anyhow::anyhow!(
            "failed to initialize store at {}: {e}",
            path.display()
        ))
    };
    match Store::open(&path) {
        Ok(_) => Ok("Opened existing store"),
        Err(StoreError::NotFound(_)) => gage_store::init(&path)
            .map(|_| "Created store")
            .map_err(init_err),
        Err(e) => Err(init_err(e)),
    }
}

fn remove_dialog(args: &InitArgs) -> Result<DialogResult, DialogError> {
    let registry = driver_registry();
    let driver = registry
        .default()
        .ok_or_else(|| DialogError::Other(anyhow::anyhow!("no default driver registered")))?;

    cli::log::step(format!("Driver\n{}", driver.name()))?;

    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(false).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let mut ui = SpinnerUi::default();
    driver
        .system_remove(&mut ui)
        .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
    ui.finish();

    Ok(DialogResult::from(format!(
        "Removed Gage from {} harness",
        driver.name()
    )))
}

/// `InstallUi` sink that drives one dialog spinner at a time: each
/// `step` starts one, `step_done` submits it as a step line, and a
/// warning raised while a spinner runs is held until the spinner
/// stops, since a log line written under an active spinner would
/// overwrite its row.
#[derive(Default)]
struct SpinnerUi {
    current: Option<(cli::ProgressBar, String)>,
    pending: Vec<String>,
}

impl SpinnerUi {
    fn finish(&mut self) {
        if let Some((spinner, label)) = self.current.take() {
            spinner.stop(label);
        }
        self.flush();
    }

    fn flush(&mut self) {
        for message in self.pending.drain(..) {
            // cliclack's `warning` writer is best-effort; a terminal
            // write failure here is not actionable at install time.
            drop(cli::log::warning(message));
        }
    }
}

impl InstallUi for SpinnerUi {
    fn step(&mut self, label: &str) {
        self.finish();
        let spinner = cli::spinner();
        spinner.start(label);
        self.current = Some((spinner, label.to_string()));
    }

    fn step_done(&mut self) {
        self.finish();
    }

    fn warn(&mut self, message: &str) {
        self.pending.push(message.to_string());
        if self.current.is_none() {
            self.flush();
        }
    }
}
