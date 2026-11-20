//! Registry of session drivers keyed by URL scheme.
//!
//! Constructed and populated explicitly at process startup by the
//! facility that needs it (the CLI, the MCP server, ...). No lazy
//! module-level init.
//!
//! The default driver is the one the empty source and an agent model
//! with no scheme go to. A process names it once at startup with
//! [`set_default_driver`], from the user's config; a process that
//! names none gets [`DEFAULT_DRIVER`]. The models the user's config
//! sets for a driver's sizes are named the same way, with
//! [`set_driver_models`].

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use gage_claude::driver::ClaudeDriver;
use gage_codex::driver::CodexDriver;
use gage_core::config::DEFAULT_DRIVER;
use gage_pi::driver::PiDriver;
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
        Arc::new(PiDriver::with_models(models_of("pi"))),
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

    /// Register `driver` and mark it as the default: the driver the
    /// empty source is handed to. Panics if a default is already set.
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

    /// The driver that handles `source` and the source to hand that
    /// driver. A source is `<scheme>:<path>` under a registered scheme.
    /// A registered scheme alone, with no colon, is that driver's
    /// default location and is returned as `<scheme>:`. The empty
    /// source is the default driver's default location. A source that
    /// names no driver is an error and so is one whose scheme no
    /// driver serves: a path alone does not say which driver reads it.
    pub fn resolve_source(&self, source: &str) -> Result<(Arc<dyn Driver>, String), DriverError> {
        if source.is_empty() {
            let driver = self
                .default()
                .ok_or_else(|| DriverError::Other("no default session driver registered".into()))?;
            return Ok((driver, String::new()));
        }
        if let Some(driver) = self.for_scheme(source) {
            return Ok((driver, format!("{source}:")));
        }
        match split_scheme(source) {
            Some((scheme, _)) => match self.for_scheme(scheme) {
                Some(driver) => Ok((driver, source.to_string())),
                None => Err(DriverError::Other(format!(
                    "no driver serves scheme {scheme:?}; schemes: {}",
                    self.schemes().join(", ")
                ))),
            },
            None => Err(DriverError::Other(format!(
                "source names no driver; write it as SCHEME:PATH, where SCHEME is one of {}",
                self.schemes().join(", ")
            ))),
        }
    }

    /// Every registered scheme, in registration order
    fn schemes(&self) -> Vec<&'static str> {
        let mut schemes = Vec::new();
        for scheme in self.drivers.iter().flat_map(|d| d.schemes()) {
            if !schemes.contains(scheme) {
                schemes.push(*scheme);
            }
        }
        schemes
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

    /// Route the source `options` names to its driver and open it, as
    /// [`Self::resolve_source`] reads it.
    pub fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
        let (driver, source) = self.resolve_source(options.url())?;
        driver.open_source(&options.with_url(&source))
    }

    /// Route `session_url` to its driver by scheme and split it into
    /// its source URL and session id. A URL with no registered scheme
    /// goes to the default driver.
    pub fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
        let driver = match split_scheme(session_url) {
            Some((scheme, _)) if self.by_scheme.contains_key(scheme) => self.for_scheme(scheme),
            _ => self.default(),
        }
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
        /// Fails with the source it was handed.
        fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other(format!("opened {}", options.url())))
        }
        /// Fails with its own name and the URL it was handed.
        fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
            Err(DriverError::Other(format!(
                "{} split {session_url}",
                self.0
            )))
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

    /// Each harness names its session where only its own driver
    /// looks, so a session goes to the driver whose harness hosts it.
    #[test]
    fn a_claude_session_goes_to_claude_and_a_codex_session_to_codex() {
        let registry = bundled_with(&HashMap::new())
            .into_iter()
            .fold(DriverRegistry::new(), DriverRegistry::add);

        let vars = [("CLAUDE_CODE_SESSION_ID", "c1")];
        let url = registry
            .current_session(&HostContext::with_vars(&vars))
            .unwrap()
            .unwrap();
        assert!(url.starts_with("claude:") && url.ends_with("/c1"), "{url}");

        let meta = serde_json::json!({"x-codex-turn-metadata": {"thread_id": "t1"}});
        let host = HostContext::with_vars(&[]).serving_call(meta.as_object().unwrap());
        let url = registry.current_session(&host).unwrap().unwrap();
        assert!(url.starts_with("codex:") && url.ends_with("/t1"), "{url}");

        let vars = [("CODEX_THREAD_ID", "t2")];
        let url = registry
            .current_session(&HostContext::with_vars(&vars))
            .unwrap()
            .unwrap();
        assert!(url.starts_with("codex:") && url.ends_with("/t2"), "{url}");
    }

    /// A Pi source is routed to the Pi driver by its scheme, and a
    /// session Pi names in the environment is the Pi driver's.
    #[test]
    fn a_pi_source_goes_to_the_pi_driver() {
        let registry = bundled_with(&HashMap::new())
            .into_iter()
            .fold(DriverRegistry::new(), DriverRegistry::add);
        let names: Vec<&str> = registry.drivers().iter().map(|d| d.name()).collect();
        assert_eq!(names, ["claude", "codex", "pi"]);
        let (driver, source) = registry.resolve_source("pi:/a/agent").unwrap();
        assert_eq!((driver.name(), source.as_str()), ("pi", "pi:/a/agent"));
        let (source, id) = registry.split_session_url("pi:/a/agent/s1").unwrap();
        assert_eq!((source.as_str(), id.as_str()), ("pi:/a/agent", "s1"));
        let pi = registry.for_name("pi").unwrap();
        assert_eq!(pi.harness_name(), "Pi");
        let vars = [("PI_SESSION_ID", "s1")];
        let url = registry
            .current_session(&HostContext::with_vars(&vars))
            .unwrap()
            .unwrap();
        assert!(url.starts_with("pi:") && url.ends_with("/s1"), "{url}");
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
        assert_eq!(registry.resolve_source("b:x").unwrap().0.name(), "two");
        assert_eq!(registry.resolve_source("shared:x").unwrap().0.name(), "one");
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

    fn resolved(registry: &DriverRegistry, source: &str) -> Result<(&'static str, String), String> {
        registry
            .resolve_source(source)
            .map(|(driver, source)| (driver.name(), source))
            .map_err(|e| e.to_string())
    }

    #[test]
    fn a_scheme_alone_is_the_driver_default_location() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        assert_eq!(resolved(&registry, "b"), Ok(("two", "b:".to_string())));
        assert_eq!(resolved(&registry, "b:"), Ok(("two", "b:".to_string())));
        assert_eq!(resolved(&registry, "b:/x"), Ok(("two", "b:/x".to_string())));
    }

    #[test]
    fn the_empty_source_is_the_default_driver_default_location() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        assert_eq!(resolved(&registry, ""), Ok(("one", String::new())));
        let none = DriverRegistry::new().add(Arc::new(Fake("two", &["b"])));
        assert!(resolved(&none, "").is_err());
    }

    #[test]
    fn a_source_that_names_no_driver_is_an_error() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        for source in ["/tmp/x", "./b", "b/x", "~/x:y", "other"] {
            let err = resolved(&registry, source).unwrap_err();
            assert!(err.contains("names no driver"), "{source}: {err}");
            assert!(err.contains("one of a, b"), "{source}: {err}");
        }
        let err = resolved(&registry, "zzz:x").unwrap_err();
        assert_eq!(err, "no driver serves scheme \"zzz\"; schemes: a, b");
    }

    /// The driver is handed the source with its scheme, so it never
    /// reads a bare scheme as a path.
    #[test]
    fn open_source_hands_the_driver_the_resolved_source() {
        let registry = DriverRegistry::new().add_default(Arc::new(Fake("one", &["a"])));
        let err = |source: &str| match registry.open_source(&SourceOptions::new(source)) {
            Ok(_) => panic!("{source}: opened"),
            Err(e) => e.to_string(),
        };
        assert_eq!(err("a"), "opened a:");
        assert_eq!(err("a:/x"), "opened a:/x");
        assert!(err("/x").contains("names no driver"));
    }

    #[test]
    fn a_session_url_with_no_registered_scheme_goes_to_default() {
        let registry = DriverRegistry::new()
            .add_default(Arc::new(Fake("one", &["a"])))
            .add(Arc::new(Fake("two", &["b"])));
        let split = |url: &str| registry.split_session_url(url).unwrap_err().to_string();
        assert_eq!(split("s1"), "one split s1");
        assert_eq!(split("zzz:s1"), "one split zzz:s1");
        assert_eq!(split("b:/x/s1"), "two split b:/x/s1");
    }
}
