use clap::Args;
use cliclack as cli;
use gage_db::db::{db_path, open_db};
use gage_session::InstallUi;
use gage_store::{Store, StoreError};
use indicatif::ProgressBar;

use crate::dialog::{self, DialogError, DialogResult};
use crate::source::driver_registry;

#[derive(Args)]
pub struct InitArgs {
    /// Uninstall Gage from the harness
    #[arg(short, long)]
    pub remove: bool,

    /// Skip confirmation prompt
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
    let registry = driver_registry();
    let driver = registry
        .default()
        .ok_or_else(|| DialogError::Other(anyhow::anyhow!("no default driver registered")))?;

    cli::log::step("Gage store")?;
    cli::log::step(format!(
        "Plugin\ngage (MCP server + skills) via driver {}",
        driver.name()
    ))?;

    if !args.yes {
        let confirmed = cli::confirm("Continue?").initial_value(true).interact()?;
        if !confirmed {
            return Err(DialogError::Canceled);
        }
    }

    let db_spinner = crate::style::spinner("Initializing database");
    let db_result = open_db();
    db_spinner.finish_and_clear();
    db_result.map_err(|e| {
        DialogError::Other(anyhow::anyhow!(
            "failed to initialize database at {}: {e}",
            db_path().display()
        ))
    })?;

    let store_spinner = crate::style::spinner("Initializing store");
    let store_result = init_store();
    store_spinner.finish_and_clear();
    store_result?;

    let gage_bin = std::env::current_exe()?;
    let gage_bin_str = gage_bin.to_str().ok_or_else(|| {
        DialogError::Other(anyhow::anyhow!(
            "gage binary path is not valid UTF-8: {}",
            gage_bin.display()
        ))
    })?;

    let mut ui = SpinnerUi::default();
    driver
        .install_gage(&[gage_bin_str, "mcp2"], &mut ui)
        .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
    ui.finish();

    Ok(DialogResult::from("Gage is initialized"))
}

/// Creates the store only when none exists. An existing store is
/// opened, which validates its version and object format without
/// running any git command that modifies it; `gage_store::init` on an
/// existing store would rewrite `gage.version` and the update hook.
fn init_store() -> Result<(), DialogError> {
    let path = gage_store::store_path();
    let init_err = |e: StoreError| {
        DialogError::Other(anyhow::anyhow!(
            "failed to initialize store at {}: {e}",
            path.display()
        ))
    };
    match Store::open(&path) {
        Ok(_) => Ok(()),
        Err(StoreError::NotFound(_)) => gage_store::init(&path).map(|_| ()).map_err(init_err),
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
        .uninstall_gage(&mut ui)
        .map_err(|e| DialogError::Other(anyhow::anyhow!("{e}")))?;
    ui.finish();

    Ok(DialogResult::from(format!(
        "Gage removed from {} harness",
        driver.name()
    )))
}

/// `InstallUi` sink that drives a single cliclack spinner at a time:
/// each `step` starts one, `step_done` clears it, and `warn` logs a
/// warning line out of band.
#[derive(Default)]
struct SpinnerUi {
    current: Option<ProgressBar>,
}

impl SpinnerUi {
    fn finish(&mut self) {
        if let Some(bar) = self.current.take() {
            bar.finish_and_clear();
        }
    }
}

impl InstallUi for SpinnerUi {
    fn step(&mut self, label: &str) {
        if let Some(bar) = self.current.take() {
            bar.finish_and_clear();
        }
        self.current = Some(crate::style::spinner(label));
    }

    fn step_done(&mut self) {
        if let Some(bar) = self.current.take() {
            bar.finish_and_clear();
        }
    }

    fn warn(&mut self, message: &str) {
        // cliclack's `warning` writer is best-effort; a terminal write
        // failure here is not actionable at install time.
        drop(cli::log::warning(message));
    }
}
