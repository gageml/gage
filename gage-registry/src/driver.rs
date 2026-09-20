//! Registry of session drivers keyed by URL scheme.
//!
//! Constructed and populated explicitly at process startup by the
//! facility that needs it (the CLI, the MCP server, ...). No lazy
//! module-level init.

use std::collections::HashMap;
use std::sync::Arc;

use gage_session::Driver;

pub struct DriverRegistry {
    default: Option<Arc<dyn Driver>>,
    by_scheme: HashMap<&'static str, Arc<dyn Driver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self {
            default: None,
            by_scheme: HashMap::new(),
        }
    }

    /// Register `driver` under its scheme and mark it as the default
    /// used when no `--source` is given. Panics if a default is
    /// already set.
    pub fn add_default(mut self, driver: Arc<dyn Driver>) -> Self {
        assert!(self.default.is_none(), "default driver already set");
        self.by_scheme.insert(driver.name(), driver.clone());
        self.default = Some(driver);
        self
    }

    /// Register `driver` under its scheme.
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, driver: Arc<dyn Driver>) -> Self {
        self.by_scheme.insert(driver.name(), driver);
        self
    }

    pub fn for_scheme(&self, scheme: &str) -> Option<Arc<dyn Driver>> {
        self.by_scheme.get(scheme).cloned()
    }

    pub fn default(&self) -> Option<Arc<dyn Driver>> {
        self.default.clone()
    }
}

impl Default for DriverRegistry {
    fn default() -> Self {
        Self::new()
    }
}
