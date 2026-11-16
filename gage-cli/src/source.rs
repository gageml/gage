//! Session source selection for the CLI: the driver registry and the
//! `--native` resolution shared by commands that read sessions.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use gage_core::config::{Config, DriversConfig, default_cache_root, display_user_config_path};
use gage_registry::driver::{DriverRegistry, set_default_driver, set_driver_models};
use gage_session::{Driver, ModelSizes, SkillSession, Source, SourceOptions};
use gage_store::{Store, StoreError};

use crate::error::CliError;

/// Long help shared by every `--native` option
pub const SOURCE_HELP: &str = "Session directory as SCHEME:PATH. A value with no scheme uses \
the default driver. Schemes are defined by drivers:

  claude  Claude Code sessions under \
PATH/projects; without PATH, the
          Claude Code home directory
  codex   Codex sessions under PATH/sessions; without PATH, the
          Codex home directory";

/// The drivers this CLI build registers. The default is the one the
/// config names, once [`configure_drivers`] has run.
pub fn driver_registry() -> DriverRegistry {
    DriverRegistry::builtin()
}

/// Configure the process's drivers from the `drivers` settings of the
/// user config: the default driver and the models each driver
/// resolves its sizes to. A config that cannot be read, or that names
/// a default driver this build does not have, is the error.
pub fn configure_drivers() -> Result<(), CliError> {
    let path = display_user_config_path();
    let config = Config::load_user().with_context(|| format!("reading {path}"))?;
    set_driver_models(driver_models(&config.drivers))
        .with_context(|| format!("{path}: drivers"))?;
    set_default_driver(&config.drivers.default)
        .with_context(|| format!("{path}: drivers.default"))
        .map_err(CliError::from)
}

/// The model sizes `config` sets, by driver name
pub fn driver_models(config: &DriversConfig) -> HashMap<String, ModelSizes> {
    config
        .entries
        .iter()
        .map(|(name, entry)| {
            let models = &entry.models;
            let sizes = ModelSizes {
                small: models.small.clone(),
                medium: models.medium.clone(),
                large: models.large.clone(),
            };
            (name.clone(), sizes)
        })
        .collect()
}

/// Resolve a `--native` value to the driver that handles it and the
/// value to hand that driver. `None` is the empty source, which every
/// driver reads as its default location.
pub fn resolve_source(
    registry: &DriverRegistry,
    source: Option<&str>,
) -> Result<(Arc<dyn Driver>, String), String> {
    let source = source.unwrap_or("");
    let driver = registry
        .driver_for(source)
        .ok_or_else(|| "no default session driver registered".to_string())?;
    Ok((driver, source.to_string()))
}

/// A source as a dialog names it: the value given, or the driver's
/// default location when none was.
pub fn describe_source(driver: &dyn Driver, spec: &str) -> String {
    if spec.is_empty() {
        format!("{} (default location)", driver.name())
    } else {
        spec.to_string()
    }
}

/// Long help shared by every `--stored-remote` option, after the
/// option's own short line
pub const STORED_REMOTE_HELP: &str = "NAME is a remote that gage fetch brought objects from. Those objects are kept apart from the store until gage merge merges them. Nothing is fetched and nothing is written";

/// Open the store a command reads: its own objects, or with
/// `stored_remote` the objects fetched from that remote, which are
/// read and never written.
pub fn open_stored(stored_remote: Option<&str>) -> Result<Store, StoreError> {
    let path = gage_store::store_path();
    match stored_remote {
        None => Store::open(&path),
        Some(remote) => Store::open_stored_remote(&path, &default_cache_root(), remote),
    }
}

/// The error of a command that writes sessions and was given
/// `--stored-remote`: `<command>: --stored-remote does not apply; ...`.
pub fn stored_remote_is_read_only(command: &str) -> CliError {
    CliError::message(format!(
        "{command}: --stored-remote does not apply; sessions fetched from a remote are read only"
    ))
}

/// The default driver, or `command: <error>`
pub fn default_driver(command: &str) -> Result<Arc<dyn Driver>, CliError> {
    driver_registry().default().ok_or_else(|| {
        CliError::message(format!("{command}: no default session driver registered"))
    })
}

/// Open `source` through the registry. `label` names the caller in the
/// error: `<label>: <source>: <error>`.
pub fn open_source(source: &str, label: &str) -> Result<Box<dyn Source>, CliError> {
    driver_registry()
        .open_source(&SourceOptions::new(source))
        .with_context(|| format!("{label}: {source}"))
        .map_err(CliError::from)
}

/// Start `session` in the default driver's harness and wait for it to
/// end. The harness owns the terminal until then. A non-zero exit is
/// the error `<command>: session exited with status <code>`.
pub fn run_skill_session(command: &str, session: &SkillSession<'_>) -> Result<(), CliError> {
    let driver = default_driver(command)?;
    let mut running = driver
        .start_skill_session(session)
        .context(command.to_string())?;
    let code = running
        .wait()
        .with_context(|| format!("{command}: waiting for the session"))?;
    if code != 0 {
        return Err(CliError::message(format!(
            "{command}: session exited with status {code}"
        )));
    }
    Ok(())
}
