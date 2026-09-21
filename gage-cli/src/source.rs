//! Session source selection for the CLI: the driver registry and the
//! `--source` resolution shared by commands that read sessions.

use std::sync::Arc;

use gage_claude::index::IndexStore;
use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, Source};

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

/// Open `source` through the registry, or print `command: <error>`
/// and exit.
pub fn open_source_or_exit(command: &str, source: &str) -> Box<dyn Source> {
    match driver_registry().open_source(source) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{command}: {source}: {e}");
            std::process::exit(1);
        }
    }
}

/// The index store behind `source`, or print `command: <error>` and
/// exit.
pub fn index_store_or_exit(command: &str, source: &dyn Source) -> Arc<IndexStore> {
    match gage_query::index_store(source) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{command}: {e}");
            std::process::exit(1);
        }
    }
}
