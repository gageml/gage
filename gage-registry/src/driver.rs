//! Registry of session drivers keyed by URL scheme.
//!
//! Constructed and populated explicitly at process startup by the
//! facility that needs it (the CLI, the MCP server, ...). No lazy
//! module-level init.
//!
//! The default driver is the one a source or an agent model with no
//! scheme goes to. A process names it once at startup with
//! [`set_default_driver`], from the user's config; a process that
//! names none gets [`DEFAULT_DRIVER`]. The models the user's config
//! sets for a driver's sizes are named the same way, with
//! [`set_driver_models`].

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use gage_claude::driver::ClaudeDriver;
use gage_codex::driver::CodexDriver;
use gage_core::config::DEFAULT_DRIVER;
use gage_session::{
    Driver, DriverError, HostContext, ModelSizes, Source, SourceOptions, split_scheme,
};

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

/// The models the process set for each driver's sizes, by driver
/// name, once set
static DRIVER_MODELS: OnceLock<HashMap<String, ModelSizes>> = OnceLock::new();

/// Set the model sizes each driver that [`DriverRegistry::builtin`]
/// builds in this process resolves in place of its defaults, by
/// driver name. A name no bundled driver has is kept and unused, so
/// one config serves builds with different drivers. The models are
/// fixed once set; setting different ones afterwards is an error.
pub fn set_driver_models(models: HashMap<String, ModelSizes>) -> Result<(), DriverError> {
    let set = DRIVER_MODELS.get_or_init(|| models.clone());
    if *set != models {
        return Err(DriverError::Other(
            "driver models are already set".to_string(),
        ));
    }
    Ok(())
}

/// The drivers bundled with this build, in registration order, each
/// resolving the model sizes the process set for it
fn bundled() -> Vec<Arc<dyn Driver>> {
    bundled_with(DRIVER_MODELS.get().unwrap_or(&HashMap::new()))
}

fn bundled_with(models: &HashMap<String, ModelSizes>) -> Vec<Arc<dyn Driver>> {
    let none = ModelSizes::default();
    let models_of = |name: &str| models.get(name).unwrap_or(&none);
    vec![
        Arc::new(ClaudeDriver::with_models(models_of("claude"))),
        Arc::new(CodexDriver::with_models(models_of("codex"))),
    ]
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

    /// The session `host` describes, as the first driver in
    /// registration order that sees one of its sessions there names
    /// it. `None` when no driver sees one. A driver that fails is
    /// passed over; its error is returned when no driver answers.
    pub fn current_session(&self, host: &HostContext<'_>) -> Result<Option<String>, DriverError> {
        let mut failed = None;
        for driver in &self.drivers {
            match driver.current_session(host) {
                Ok(Some(url)) => return Ok(Some(url)),
                Ok(None) => {}
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        failed.map_or(Ok(None), Err)
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
        /// The session a tool call names under this driver's key,
        /// `<name>/session`; a `<name>/fail` key is a failure.
        fn current_session(&self, host: &HostContext<'_>) -> Result<Option<String>, DriverError> {
            let Some(meta) = host.call_meta() else {
                return Ok(None);
            };
            if meta.contains_key(&format!("{}/fail", self.0)) {
                return Err(DriverError::Other(format!("{} failed", self.0)));
            }
            Ok(meta
                .get(&format!("{}/session", self.0))
                .and_then(|id| id.as_str())
                .map(|id| format!("{}:{id}", self.0)))
        }
    }

    fn current(
        registry: &DriverRegistry,
        meta: serde_json::Value,
    ) -> Result<Option<String>, String> {
        let host = HostContext::with_vars(&[]);
        registry
            .current_session(&host.serving_call(meta.as_object().unwrap()))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn current_session_is_the_first_driver_that_sees_one() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        let url = current(&registry, serde_json::json!({"two/session": "t1"}));
        assert_eq!(url.unwrap().as_deref(), Some("two:t1"));
        let url = current(
            &registry,
            serde_json::json!({"one/session": "o1", "two/session": "t1"}),
        );
        assert_eq!(url.unwrap().as_deref(), Some("one:o1"));
        assert_eq!(current(&registry, serde_json::json!({})).unwrap(), None);
        let host = HostContext::with_vars(&[]);
        assert_eq!(registry.current_session(&host).unwrap(), None);
    }

    #[test]
    fn current_session_passes_over_a_failing_driver() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        let url = current(
            &registry,
            serde_json::json!({"one/fail": true, "two/session": "t1"}),
        );
        assert_eq!(url.unwrap().as_deref(), Some("two:t1"));
        let err = current(&registry, serde_json::json!({"one/fail": true})).unwrap_err();
        assert!(err.contains("one failed"), "{err}");
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
    fn a_bundled_driver_resolves_the_sizes_set_for_it_and_defaults_the_rest() {
        let unset = bundled_with(&HashMap::new());
        let defaults = unset[0].model_sizes();
        assert!(defaults.small.is_some() && defaults.medium.is_some());

        let set = ModelSizes {
            medium: Some("custom-medium".to_string()),
            ..ModelSizes::default()
        };
        let models = HashMap::from([
            ("claude".to_string(), set),
            ("not-in-this-build".to_string(), ModelSizes::default()),
        ]);
        let sizes = bundled_with(&models)[0].model_sizes();
        assert_eq!(sizes.medium.as_deref(), Some("custom-medium"));
        assert_eq!(sizes.small, defaults.small);
        assert_eq!(sizes.large, defaults.large);
    }

    #[test]
    fn each_bundled_driver_takes_the_sizes_set_under_its_own_name() {
        let set = ModelSizes {
            large: Some("codex-large".to_string()),
            ..ModelSizes::default()
        };
        let drivers = bundled_with(&HashMap::from([("codex".to_string(), set)]));
        let sizes_of = |name: &str| {
            drivers
                .iter()
                .find(|d| d.name() == name)
                .expect("a bundled driver")
                .model_sizes()
        };
        assert_eq!(sizes_of("codex").large.as_deref(), Some("codex-large"));
        assert!(sizes_of("codex").small.is_some());
        assert_ne!(sizes_of("claude").large.as_deref(), Some("codex-large"));
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
