//! Registry of session drivers keyed by URL scheme.
//!
//! Constructed and populated explicitly at process startup by the
//! facility that needs it (the CLI, the MCP server, ...). No lazy
//! module-level init.

use std::collections::HashMap;
use std::sync::Arc;

use gage_claude::driver::ClaudeDriver;
use gage_session::{Driver, DriverError, Source, split_scheme};

pub struct DriverRegistry {
    default: Option<Arc<dyn Driver>>,
    /// Each scheme maps to the first added driver that serves it
    by_scheme: HashMap<&'static str, Arc<dyn Driver>>,
    /// Every added driver, in registration order
    drivers: Vec<Arc<dyn Driver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self {
            default: None,
            by_scheme: HashMap::new(),
            drivers: Vec::new(),
        }
    }

    /// The drivers bundled with this build. `claude` is the default.
    pub fn builtin() -> Self {
        Self::new().add_default(Arc::new(ClaudeDriver::new()))
    }

    /// Register `driver` and mark it as the default: the driver a
    /// scheme-less source is handed to. Panics if a default is already
    /// set.
    pub fn add_default(mut self, driver: Arc<dyn Driver>) -> Self {
        assert!(self.default.is_none(), "default driver already set");
        self.default = Some(driver.clone());
        self.add(driver)
    }

    /// Register `driver` under each scheme it serves that no earlier
    /// driver claimed.
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, driver: Arc<dyn Driver>) -> Self {
        for scheme in driver.schemes() {
            self.by_scheme
                .entry(scheme)
                .or_insert_with(|| driver.clone());
        }
        self.drivers.push(driver);
        self
    }

    pub fn for_scheme(&self, scheme: &str) -> Option<Arc<dyn Driver>> {
        self.by_scheme.get(scheme).cloned()
    }

    /// The first added driver whose [`Driver::name`] is `name`. This
    /// is the lookup for a stored session, which records its driver's
    /// name rather than a scheme.
    pub fn for_name(&self, name: &str) -> Option<Arc<dyn Driver>> {
        self.drivers.iter().find(|d| d.name() == name).cloned()
    }

    pub fn default(&self) -> Option<Arc<dyn Driver>> {
        self.default.clone()
    }

    /// The driver that handles `source`: the one registered for its
    /// scheme when it has a registered scheme, else the default.
    pub fn driver_for(&self, source: &str) -> Option<Arc<dyn Driver>> {
        match split_scheme(source) {
            Some((scheme, _)) if self.by_scheme.contains_key(scheme) => self.for_scheme(scheme),
            _ => self.default(),
        }
    }

    /// Route `source` to its driver and open it. The empty source is
    /// the default driver's default location.
    pub fn open_source(&self, source: &str) -> Result<Box<dyn Source>, DriverError> {
        let driver = self
            .driver_for(source)
            .ok_or_else(|| DriverError::Other("no default session driver registered".into()))?;
        driver.open_source(source)
    }
}

impl Default for DriverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gage_session::{
        ContentSink, ContentSource, DriverError, NativeSession, Source, StoredSession,
    };

    struct Fake(&'static str, &'static [&'static str]);

    impl Driver for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn schemes(&self) -> &'static [&'static str] {
            self.1
        }
        fn open_source(&self, _source: &str) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            _sink: &mut dyn ContentSink,
        ) -> Result<String, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
    }

    #[test]
    fn scheme_routes_to_first_driver_that_serves_it() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a", "shared"])))
            .add(Arc::new(Fake("two", &["b", "shared"])));
        assert_eq!(registry.driver_for("b:x").unwrap().name(), "two");
        assert_eq!(registry.driver_for("shared:x").unwrap().name(), "one");
    }

    #[test]
    fn name_lookup_finds_any_added_driver() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        assert_eq!(registry.for_name("two").unwrap().name(), "two");
        assert!(registry.for_name("three").is_none());
    }

    #[test]
    fn scheme_less_and_unknown_scheme_go_to_default() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        assert_eq!(registry.driver_for("").unwrap().name(), "one");
        assert_eq!(registry.driver_for("/tmp/x").unwrap().name(), "one");
        assert_eq!(registry.driver_for("zzz:x").unwrap().name(), "one");
    }
}
