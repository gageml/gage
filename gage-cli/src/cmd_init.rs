use std::sync::Arc;

use clap::Args;
use cliclack as cli;
use gage_core::config::{Config, user_config_path};
use gage_registry::driver::DriverRegistry;
use gage_session::{Driver, DriverStatus, Field, InstallUi};
use gage_store::{Store, StoreError};

use crate::cmd_scanner;
use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::source::driver_registry;

#[derive(Args)]
pub struct InitArgs {
    /// Remove Gage from coding agents
    #[arg(short, long, conflicts_with = "default")]
    pub remove: bool,

    /// Driver to set up (repeatable)
    ///
    /// Without this option Gage is set up in every installed coding
    /// agent it has a driver for.
    #[arg(short, long = "driver", value_name = "DRIVER")]
    pub drivers: Vec<String>,

    /// Default driver
    ///
    /// A command that names no session source reads this driver, and a
    /// model with no driver named goes to it. Without this option a
    /// default already in config is kept. With none in config, a single
    /// driver set up becomes default, and with more than one, prompt
    /// asks which.
    #[arg(long, value_name = "DRIVER")]
    pub default: Option<String>,

    /// Leave scanners uninstalled
    ///
    /// Without this option, an init that finds no installed scanner
    /// asks whether to install scanners that Gage publishes, and
    /// installs them under --yes.
    #[arg(long, conflicts_with = "remove")]
    pub no_scanners: bool,

    /// Skip prompts
    ///
    /// With more than one driver set up and no default in config,
    /// --default is required.
    #[arg(short, long)]
    pub yes: bool,
}

pub fn run(args: InitArgs) -> Result<(), CliError> {
    if args.remove {
        dialog::run("Remove Gage setup", || remove_dialog(&args))
    } else {
        dialog::run("Setup Gage", || install_dialog(&args))
    }
}

fn install_dialog(args: &InitArgs) -> Result<DialogResult, DialogError> {
    let registry = driver_registry();
    let config_path = user_config_path();
    let configured = Config::named_default_driver(&config_path)
        .map_err(|e| DialogError::Other(anyhow::anyhow!("reading config: {e}")))?;
    let plan = plan(
        &candidates(&registry),
        &args.drivers,
        configured.as_deref(),
        args.default.as_deref(),
        args.yes,
    )
    .map_err(DialogError::Failed)?;

    cli::log::step("Gage store")?;
    dialog::step_list(
        "Plugin (MCP server + skills)",
        plan.drivers.iter().map(|name| harness_of(&registry, name)),
    )?;
    let default = match plan.default {
        DefaultPlan::Keep(name) => {
            cli::log::step(format!("Default driver\n{name}"))?;
            None
        }
        DefaultPlan::Set(name) => {
            cli::log::step(format!("Default driver\n{name}"))?;
            Some(name)
        }
        DefaultPlan::Ask(names) => Some(prompt_default(&registry, &names)?),
    };
    let scanners = plan_scanners(
        cmd_scanner::none_installed(),
        args.no_scanners,
        args.yes,
        || {
            Ok(cli::confirm("Install scanners?")
                .initial_value(true)
                .interact()?)
        },
    )?;
    if scanners {
        dialog::step_value("Scanners", "Every scanner Gage publishes")?;
    }

    dialog::confirm(args.yes, true)?;

    let spinner = cli::spinner();
    spinner.start("Initializing store");
    match init_store() {
        Ok(Some(done)) => spinner.stop(done),
        Ok(None) => spinner.clear(),
        Err(e) => {
            spinner.clear();
            return Err(e);
        }
    }

    if let Some(name) = default {
        Config::set_default_driver(&config_path, &name)
            .map_err(|e| DialogError::Other(anyhow::anyhow!("writing config: {e}")))?;
    }

    let gage_bin = std::env::current_exe()?;
    let gage_bin_str = gage_bin.to_str().ok_or_else(|| {
        DialogError::Other(anyhow::anyhow!(
            "gage binary path is not valid UTF-8: {}",
            gage_bin.display()
        ))
    })?;

    for driver in drivers_named(&registry, &plan.drivers) {
        let mut ui = SpinnerUi::start(format!("Installing support for {}", driver.harness_name()));
        driver
            .system_init(&[gage_bin_str, "mcp"], &mut ui)
            .map_err(|e| DialogError::Other(anyhow::anyhow!("{}: {e}", driver.harness_name())))?;
        ui.finish();
    }

    if scanners {
        // The rest of the setup stands without scanners, and the
        // warning names the command that installs them later
        let config = Config::load_user()
            .map_err(|e| DialogError::Other(anyhow::anyhow!("reading config: {e}")))?;
        match cmd_scanner::install_gage_scanners(&config) {
            Ok(done) => cli::log::success(done)?,
            Err(DialogError::Failed(reason)) => cli::log::warning(format!(
                "Scanners were not installed: {reason}\n\
                 Install one later with `gage scanner install`"
            ))?,
            Err(e) => return Err(e),
        }
    }

    Ok(DialogResult::from("Initialized Gage"))
}

/// Decide whether an init installs scanners. It does when none is
/// installed and `--no-scanners` is not given: under `--yes` without
/// asking, and otherwise when the user agrees to `ask`.
fn plan_scanners(
    none_installed: bool,
    no_scanners: bool,
    yes: bool,
    ask: impl FnOnce() -> Result<bool, DialogError>,
) -> Result<bool, DialogError> {
    if !none_installed || no_scanners {
        return Ok(false);
    }
    if yes {
        return Ok(true);
    }
    ask()
}

/// A driver as `gage init` weighs it
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    /// The driver's name
    name: String,
    /// The harness's name as a person knows it
    harness: String,
    /// The harness is installed, so Gage can be set up in it
    installed: bool,
}

/// What an init does: the drivers it sets Gage up in, by name, and
/// what becomes of the default driver.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    drivers: Vec<String>,
    default: DefaultPlan,
}

#[derive(Debug, PartialEq, Eq)]
enum DefaultPlan {
    /// The config names this driver and it stays
    Keep(String),
    /// This driver is written to the config
    Set(String),
    /// The user chooses among these
    Ask(Vec<String>),
}

/// Decide what an init does. `only` is the `--driver` values,
/// `configured` the default driver the config names, `requested` the
/// `--default` value, and `yes` whether prompts are skipped.
///
/// The drivers set up are the installed ones, narrowed to `only` when
/// it names any. The default driver is `requested` when given; else
/// the configured one, so that running init again changes nothing the
/// user did not ask for; else the one driver set up; else the user's
/// choice among those set up, which `yes` cannot make. The error is
/// the message for the user.
fn plan(
    candidates: &[Candidate],
    only: &[String],
    configured: Option<&str>,
    requested: Option<&str>,
    yes: bool,
) -> Result<Plan, String> {
    for name in only.iter().map(String::as_str).chain(requested) {
        installed(candidates, name)?;
    }
    let drivers: Vec<String> = candidates
        .iter()
        .filter(|c| c.installed && (only.is_empty() || only.contains(&c.name)))
        .map(|c| c.name.clone())
        .collect();
    if drivers.is_empty() {
        let harnesses: Vec<&str> = candidates.iter().map(|c| c.harness.as_str()).collect();
        return Err(format!(
            "No coding agent Gage supports is installed. Install one of these and run \
             `gage init` again: {}",
            harnesses.join(", ")
        ));
    }
    let default = match (requested, configured, drivers.as_slice()) {
        (Some(requested), Some(configured), _) if requested == configured => {
            DefaultPlan::Keep(configured.to_string())
        }
        (Some(requested), _, _) => DefaultPlan::Set(requested.to_string()),
        (None, Some(configured), _) => DefaultPlan::Keep(configured.to_string()),
        (None, None, [only]) => DefaultPlan::Set(only.clone()),
        (None, None, _) if yes => {
            return Err(format!(
                "More than one driver is set up ({}) and config names no default driver. \
                 Name one with --default.",
                drivers.join(", ")
            ));
        }
        (None, None, _) => DefaultPlan::Ask(drivers.clone()),
    };
    Ok(Plan { drivers, default })
}

/// Check that `name` is a driver whose harness is installed.
fn installed(candidates: &[Candidate], name: &str) -> Result<(), String> {
    let Some(candidate) = candidates.iter().find(|c| c.name == name) else {
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        return Err(format!(
            "{name:?} is not a driver Gage can be set up in; drivers: {}",
            names.join(", ")
        ));
    };
    if !candidate.installed {
        return Err(format!(
            "{} is not installed, so the {name} driver cannot be set up or made default.",
            candidate.harness
        ));
    }
    Ok(())
}

/// Every driver of the build whose harness Gage can be set up in,
/// with whether that harness is installed
fn candidates(registry: &DriverRegistry) -> Vec<Candidate> {
    registry
        .drivers()
        .iter()
        .filter(|driver| has_harness(driver.as_ref()))
        .map(|driver| Candidate {
            name: driver.name().to_string(),
            harness: driver.harness_name().to_string(),
            installed: driver.harness_path().is_some(),
        })
        .collect()
}

/// Whether `driver` has a harness for Gage to be set up in. A driver
/// that only reads and stores a harness's sessions names no harness
/// path and reports no harness in its status; one whose harness is
/// not installed reports it absent.
fn has_harness(driver: &dyn Driver) -> bool {
    driver.harness_path().is_some()
        || !matches!(
            driver.status(),
            Ok(DriverStatus {
                harness: Field::Unsupported,
                ..
            })
        )
}

fn drivers_named<'a>(
    registry: &'a DriverRegistry,
    names: &'a [String],
) -> impl Iterator<Item = &'a Arc<dyn Driver>> {
    registry
        .drivers()
        .iter()
        .filter(|driver| names.iter().any(|name| name == driver.name()))
}

/// The harness of the driver `name`, with the driver's name
fn harness_of(registry: &DriverRegistry, name: &str) -> String {
    match registry.for_name(name) {
        Some(driver) => format!("{} ({name} driver)", driver.harness_name()),
        None => name.to_string(),
    }
}

/// Ask which of the drivers `names` is the default.
fn prompt_default(registry: &DriverRegistry, names: &[String]) -> Result<String, DialogError> {
    let mut select = cli::select("Default driver");
    for name in names {
        let harness = registry
            .for_name(name)
            .map_or(name.as_str(), |driver| driver.harness_name());
        select = select.item(
            name.clone(),
            name,
            format!("{harness} sessions and models when no driver is named"),
        );
    }
    Ok(select.interact()?)
}

/// Creates the store only when none exists. An existing store is
/// opened, which validates its version and object format without
/// running any git command that modifies it; `gage_store::init` on an
/// existing store would rewrite `gage.version` and the update hook.
/// Returns the line that records a store created, and `None` for an
/// existing store, which the dialog does not report.
fn init_store() -> Result<Option<&'static str>, DialogError> {
    let path = gage_store::store_path();
    let init_err = |e: StoreError| {
        DialogError::Other(anyhow::anyhow!(
            "failed to initialize store at {}: {e}",
            path.display()
        ))
    };
    match Store::open(&path) {
        Ok(_) => Ok(None),
        Err(StoreError::NotFound(_)) => gage_store::init(&path)
            .map(|_| Some("Created store"))
            .map_err(init_err),
        Err(e) => Err(init_err(e)),
    }
}

/// Remove Gage from every installed harness, or from those `--driver`
/// names. The store and the config stay.
fn remove_dialog(args: &InitArgs) -> Result<DialogResult, DialogError> {
    let registry = driver_registry();
    let candidates = candidates(&registry);
    for name in &args.drivers {
        installed(&candidates, name).map_err(DialogError::Failed)?;
    }
    let names: Vec<String> = candidates
        .iter()
        .filter(|c| c.installed && (args.drivers.is_empty() || args.drivers.contains(&c.name)))
        .map(|c| c.name.clone())
        .collect();
    if names.is_empty() {
        return Err(DialogError::Failed(
            "No coding agent Gage supports is installed.".to_string(),
        ));
    }

    dialog::step_list(
        "Remove Gage from",
        names.iter().map(|name| harness_of(&registry, name)),
    )?;

    dialog::confirm(args.yes, false)?;

    for driver in drivers_named(&registry, &names) {
        let mut ui = SpinnerUi::start(format!("Removing support for {}", driver.harness_name()));
        driver
            .system_remove(&mut ui)
            .map_err(|e| DialogError::Other(anyhow::anyhow!("{}: {e}", driver.harness_name())))?;
        ui.finish();
    }

    Ok(DialogResult::from("Removed Gage"))
}

/// `InstallUi` sink that shows one spinner for the whole of a
/// driver's install or removal. The spinner shows the label of the
/// driver's current step and leaves its own label when it stops.
/// A warning is held until the spinner stops, since a log line written
/// under an active spinner would overwrite its row.
struct SpinnerUi {
    spinner: cli::ProgressBar,
    label: String,
    pending: Vec<String>,
}

impl SpinnerUi {
    /// Start the spinner with `label`, which stays as the step line
    /// once the spinner stops.
    fn start(label: String) -> Self {
        let spinner = cli::spinner();
        spinner.start(&label);
        Self {
            spinner,
            label,
            pending: Vec::new(),
        }
    }

    fn finish(self) {
        self.spinner.stop(self.label);
        for message in self.pending {
            // cliclack's `warning` writer is best-effort; a terminal
            // write failure here is not actionable at install time.
            drop(cli::log::warning(message));
        }
    }
}

impl InstallUi for SpinnerUi {
    fn step_start(&mut self, label: &str) {
        self.spinner.set_message(label);
    }

    // The next step or the end of the install replaces the text.
    fn step_done(&mut self) {}

    fn warn(&mut self, message: &str) {
        self.pending.push(message.to_string());
    }
}

#[cfg(test)]
mod tests {
    use gage_session::{
        ContentSink, ContentSource, DriverError, NativeSession, Source, SourceOptions,
        StoredSession, WrittenContent,
    };

    use super::*;

    #[test]
    fn scanners_are_installed_when_none_is_and_the_user_agrees() {
        let ask = |answer: bool| move || Ok(answer);
        let unasked = || -> Result<bool, DialogError> { panic!("the user was asked") };
        let plan =
            |none_installed, no_scanners, yes, ask: &dyn Fn() -> Result<bool, DialogError>| {
                plan_scanners(none_installed, no_scanners, yes, ask)
                    .unwrap_or_else(|_| panic!("the scanner plan failed"))
            };

        assert!(plan(true, false, false, &ask(true)));
        assert!(!plan(true, false, false, &ask(false)));
        // --yes installs without asking
        assert!(plan(true, false, true, &unasked));
        // Installed scanners and --no-scanners leave the question out
        assert!(!plan(false, false, false, &unasked));
        assert!(!plan(false, false, true, &unasked));
        assert!(!plan(true, true, false, &unasked));
        assert!(!plan(true, true, true, &unasked));
    }

    fn candidate(name: &str, harness: &str, installed: bool) -> Candidate {
        Candidate {
            name: name.to_string(),
            harness: harness.to_string(),
            installed,
        }
    }

    /// A driver that reads and stores a harness's sessions and sets
    /// Gage up in nothing
    struct ReadOnly;

    impl Driver for ReadOnly {
        fn name(&self) -> &'static str {
            "readonly"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["readonly"]
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

    /// Every bundled driver has a harness Gage is set up in.
    #[test]
    fn every_bundled_driver_is_a_candidate() {
        let registry = DriverRegistry::builtin();
        let names: Vec<String> = candidates(&registry).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["claude", "codex", "pi"]);
    }

    #[test]
    fn a_driver_with_no_harness_to_set_up_is_not_a_candidate() {
        let registry = DriverRegistry::new().add(Arc::new(ReadOnly));
        assert!(registry.for_name("readonly").is_some());
        assert!(candidates(&registry).is_empty());
    }

    fn both() -> Vec<Candidate> {
        vec![
            candidate("claude", "Claude Code", true),
            candidate("codex", "Codex", true),
        ]
    }

    fn codex_only() -> Vec<Candidate> {
        vec![
            candidate("claude", "Claude Code", false),
            candidate("codex", "Codex", true),
        ]
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn every_installed_driver_is_set_up_when_none_is_named() {
        let made = plan(&both(), &[], Some("claude"), None, true).unwrap();
        assert_eq!(made.drivers, ["claude", "codex"]);
        let made = plan(&codex_only(), &[], None, None, true).unwrap();
        assert_eq!(made.drivers, ["codex"]);
    }

    #[test]
    fn named_drivers_limit_what_is_set_up() {
        let made = plan(&both(), &names(&["codex"]), Some("claude"), None, true).unwrap();
        assert_eq!(made.drivers, ["codex"]);
        assert_eq!(made.default, DefaultPlan::Keep("claude".to_string()));
    }

    /// Running init again is an edit: a default in config stays, with
    /// or without `-y`, however many drivers are set up.
    #[test]
    fn a_configured_default_is_kept_and_nothing_is_asked() {
        for yes in [true, false] {
            let made = plan(&both(), &[], Some("codex"), None, yes).unwrap();
            assert_eq!(made.default, DefaultPlan::Keep("codex".to_string()));
        }
    }

    #[test]
    fn with_no_default_in_config_a_single_driver_becomes_default() {
        let made = plan(&codex_only(), &[], None, None, true).unwrap();
        assert_eq!(made.default, DefaultPlan::Set("codex".to_string()));
        let made = plan(&both(), &names(&["codex"]), None, None, false).unwrap();
        assert_eq!(made.default, DefaultPlan::Set("codex".to_string()));
    }

    #[test]
    fn with_no_default_in_config_and_two_drivers_the_user_is_asked() {
        let made = plan(&both(), &[], None, None, false).unwrap();
        assert_eq!(made.default, DefaultPlan::Ask(names(&["claude", "codex"])));
    }

    #[test]
    fn with_yes_two_drivers_and_no_default_in_config_a_default_is_required() {
        let err = plan(&both(), &[], None, None, true).unwrap_err();
        assert!(err.contains("--default"), "{err}");
        assert!(err.contains("claude, codex"), "{err}");
        let made = plan(&both(), &[], None, Some("codex"), true).unwrap();
        assert_eq!(made.default, DefaultPlan::Set("codex".to_string()));
    }

    #[test]
    fn a_requested_default_replaces_a_configured_one() {
        let made = plan(&both(), &[], Some("claude"), Some("codex"), true).unwrap();
        assert_eq!(made.default, DefaultPlan::Set("codex".to_string()));
        let made = plan(&both(), &[], Some("codex"), Some("codex"), true).unwrap();
        assert_eq!(made.default, DefaultPlan::Keep("codex".to_string()));
    }

    /// The default need not be among the drivers this run sets up.
    #[test]
    fn a_requested_default_may_be_a_driver_not_set_up_this_time() {
        let made = plan(&both(), &names(&["codex"]), None, Some("claude"), true).unwrap();
        assert_eq!(made.drivers, ["codex"]);
        assert_eq!(made.default, DefaultPlan::Set("claude".to_string()));
    }

    #[test]
    fn no_installed_driver_is_an_error() {
        let none = vec![
            candidate("claude", "Claude Code", false),
            candidate("codex", "Codex", false),
        ];
        let err = plan(&none, &[], None, None, false).unwrap_err();
        assert!(err.contains("Claude Code, Codex"), "{err}");
    }

    #[test]
    fn an_unknown_or_uninstalled_driver_is_an_error() {
        let err = plan(&both(), &names(&["cursor"]), None, None, true).unwrap_err();
        assert!(err.contains("not a driver Gage can be set up in"), "{err}");
        let err = plan(&codex_only(), &names(&["claude"]), None, None, true).unwrap_err();
        assert!(err.contains("Claude Code is not installed"), "{err}");
        let err = plan(&codex_only(), &[], None, Some("claude"), true).unwrap_err();
        assert!(err.contains("Claude Code is not installed"), "{err}");
        let err = plan(&both(), &[], None, Some("nope"), true).unwrap_err();
        assert!(err.contains("not a driver Gage can be set up in"), "{err}");
    }
}
