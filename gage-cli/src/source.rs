//! Session source selection for the CLI: the driver registry and the
//! `--source` resolution shared by commands that read sessions.

use std::sync::Arc;

use anyhow::Context;
use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, Source};

use crate::error::CliError;

/// Long help shared by every `--source` option
pub const SOURCE_HELP: &str = "Session directory as SCHEME:PATH. A value with no scheme uses \
the default driver. Schemes are defined by drivers:

  claude  Claude Code sessions under \
PATH/projects; without PATH, the
          Claude Code home directory";

/// The drivers this CLI build registers
pub fn driver_registry() -> DriverRegistry {
    DriverRegistry::builtin()
}

/// Resolve a `--source` value to the driver that handles it and the
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

/// The default driver, or `command: <error>`
pub fn default_driver(command: &str) -> Result<Arc<dyn Driver>, CliError> {
    driver_registry().default().ok_or_else(|| {
        CliError::message(format!("{command}: no default session driver registered"))
    })
}

/// Open `source` through the registry, or `command: source: <error>`
pub fn open_source(command: &str, source: &str) -> Result<Box<dyn Source>, CliError> {
    driver_registry()
        .open_source(source)
        .with_context(|| format!("{command}: {source}"))
        .map_err(CliError::from)
}
