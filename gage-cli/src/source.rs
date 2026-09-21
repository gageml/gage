//! Session source selection for the CLI: the driver registry and the
//! `--source` resolution shared by session commands.

use std::sync::Arc;

use gage_claude::driver::ClaudeDriver;
use gage_registry::driver::DriverRegistry;
use gage_session::Driver;

/// The drivers this CLI build registers. `claude` is the default.
pub fn driver_registry() -> DriverRegistry {
    DriverRegistry::new().add_default(Arc::new(ClaudeDriver::new()))
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
