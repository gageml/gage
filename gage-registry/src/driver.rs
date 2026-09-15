//! Registry of session drivers keyed by URL scheme.
//!
//! Constructed and populated explicitly at process startup by the
//! facility that needs it (the CLI, the MCP server, …). No lazy
//! module-level init.

use std::collections::HashMap;
use std::sync::Arc;

use gage_session::Driver;

/// Registered drivers keyed by their URL scheme name.
#[derive(Default)]
pub struct DriverRegistry {
    drivers: HashMap<&'static str, Arc<dyn Driver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `driver`. Returns the previous entry if the scheme was
    /// already registered.
    pub fn register(&mut self, driver: Arc<dyn Driver>) -> Option<Arc<dyn Driver>> {
        self.drivers.insert(driver.name(), driver)
    }

    /// Look up a driver by scheme name.
    pub fn get(&self, scheme: &str) -> Option<Arc<dyn Driver>> {
        self.drivers.get(scheme).cloned()
    }

    /// Every registered scheme name, in registration-independent order.
    pub fn schemes(&self) -> Vec<&'static str> {
        self.drivers.keys().copied().collect()
    }
}
