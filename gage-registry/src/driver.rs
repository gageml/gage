//! Registry of session drivers keyed by URL scheme.
//!
//! Constructed and populated explicitly at process startup by the
//! facility that needs it (the CLI, the MCP server, ...). No lazy
//! module-level init.
//!
//! The default driver is the one a source or an agent model with no
//! scheme goes to. A process names it once at startup with
//! [`set_default_driver`], from the user's config; a process that
//! names none gets [`DEFAULT_DRIVER`].

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use gage_claude::driver::ClaudeDriver;
use gage_core::config::DEFAULT_DRIVER;
use gage_session::{Driver, DriverError, Source, SourceOptions, split_scheme};

/// The default driver's name for this process, once named
static DEFAULT_NAME: OnceLock<String> = OnceLock::new();

/// Name the default driver of every registry [`DriverRegistry::builtin`]
/// builds in this process. `name` must be a bundled driver. The name
/// is fixed once set; naming a different one afterwards is an error.
pub fn set_default_driver(name: &str) -> Result<(), DriverError> {
    let drivers = bundled();
    if !drivers.iter().any(|d| d.name() == name) {
        let names: Vec<&str> = drivers.iter().map(|d| d.name()).collect();
        return Err(DriverError::Other(format!(
            "{name:?} is not a driver in this build; drivers: {}",
            names.join(", ")
        )));
    }
    let set = DEFAULT_NAME.get_or_init(|| name.to_string());
    if set != name {
        return Err(DriverError::Other(format!(
            "default driver is already {set:?}"
        )));
    }
    Ok(())
}

/// The drivers bundled with this build, in registration order
fn bundled() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(ClaudeDriver::new())]
}

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

    /// The drivers bundled with this build. The default is the one
    /// the process named with [`set_default_driver`], else
    /// [`DEFAULT_DRIVER`].
    pub fn builtin() -> Self {
        let default = DEFAULT_NAME.get().map_or(DEFAULT_DRIVER, String::as_str);
        bundled().into_iter().fold(Self::new(), |registry, driver| {
            if driver.name() == default {
                registry.add_default(driver)
            } else {
                registry.add(driver)
            }
        })
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

    /// Every registered driver, in registration order.
    pub fn drivers(&self) -> &[Arc<dyn Driver>] {
        &self.drivers
    }

    /// The driver that handles `source`: the one registered for its
    /// scheme when it has a registered scheme, else the default.
    pub fn driver_for(&self, source: &str) -> Option<Arc<dyn Driver>> {
        match split_scheme(source) {
            Some((scheme, _)) if self.by_scheme.contains_key(scheme) => self.for_scheme(scheme),
            _ => self.default(),
        }
    }

    /// The driver that runs an agent whose model is `model`, and the
    /// model that driver is given. A model is `<scheme>:<model>` under
    /// a registered scheme, which names the driver and leaves it the
    /// rest, or a model with no such scheme, which goes to the default
    /// driver whole. A scheme alone, `<scheme>:`, is that driver's own
    /// choice of model, as no model is the default driver's.
    pub fn agent_driver(
        &self,
        model: Option<&str>,
    ) -> Result<(Arc<dyn Driver>, Option<String>), DriverError> {
        if let Some((scheme, rest)) = model.and_then(split_scheme)
            && let Some(driver) = self.for_scheme(scheme)
        {
            let model = (!rest.is_empty()).then(|| rest.to_string());
            return Ok((driver, model));
        }
        let driver = self
            .default()
            .ok_or_else(|| DriverError::Other("no default session driver registered".into()))?;
        Ok((driver, model.map(str::to_string)))
    }

    /// Route the source `options` names to its driver and open it. The
    /// empty source is the default driver's default location.
    pub fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
        let driver = self
            .driver_for(options.url())
            .ok_or_else(|| DriverError::Other("no default session driver registered".into()))?;
        driver.open_source(options)
    }

    /// Route `session_url` to its driver by scheme and split it into
    /// its source URL and session id. A scheme-less URL goes to the
    /// default driver.
    pub fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
        let driver = self
            .driver_for(session_url)
            .ok_or_else(|| DriverError::Other("no default session driver registered".into()))?;
        driver.split_session_url(session_url)
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
        ContentSink, ContentSource, DriverError, NativeSession, Source, SourceOptions,
        StoredSession, WrittenContent,
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
        fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn split_session_url(&self, _session_url: &str) -> Result<(String, String), DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            _sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
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
    fn agent_model_scheme_names_the_driver_and_leaves_it_the_rest() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        let (driver, model) = registry.agent_driver(Some("b:medium")).unwrap();
        assert_eq!(driver.name(), "two");
        assert_eq!(model.as_deref(), Some("medium"));
        let (driver, model) = registry.agent_driver(Some("a:x:y")).unwrap();
        assert_eq!(driver.name(), "one");
        assert_eq!(model.as_deref(), Some("x:y"));
        let (driver, model) = registry.agent_driver(Some("b:")).unwrap();
        assert_eq!(driver.name(), "two");
        assert_eq!(model, None);
    }

    #[test]
    fn agent_model_without_a_driver_scheme_goes_to_default_whole() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        for model in ["medium", "llama3:8b", "ft:gpt-4o:org::id", "sonnet[1m]"] {
            let (driver, given) = registry.agent_driver(Some(model)).unwrap();
            assert_eq!(driver.name(), "one");
            assert_eq!(given.as_deref(), Some(model));
        }
        let (driver, model) = registry.agent_driver(None).unwrap();
        assert_eq!(driver.name(), "one");
        assert_eq!(model, None);
    }

    #[test]
    fn agent_model_needs_a_default_driver_when_it_names_none() {
        let registry = DriverRegistry::new().add(Arc::new(Fake("two", &["b"])));
        assert!(registry.agent_driver(Some("medium")).is_err());
        assert!(registry.agent_driver(Some("b:medium")).is_ok());
    }

    #[test]
    fn builtin_default_is_the_build_default_until_a_process_names_one() {
        assert_eq!(
            DriverRegistry::builtin().default().unwrap().name(),
            DEFAULT_DRIVER
        );
        assert!(set_default_driver("no-such-driver").is_err());
        set_default_driver(DEFAULT_DRIVER).unwrap();
        assert_eq!(
            DriverRegistry::builtin().default().unwrap().name(),
            DEFAULT_DRIVER
        );
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
