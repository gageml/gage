//! Session source selection for the CLI: the driver registry and the
//! `--native` resolution shared by commands that read sessions.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use gage_core::config::{
    Config, DriversConfig, ProjectConfig, default_cache_root, display_user_config_path,
    project_config_paths,
};
use gage_core::path::shorten_home;
use gage_registry::driver::{
    DriverRegistry, set_default_driver, set_driver_models, set_native_names,
};
use gage_session::{Driver, ModelSizes, SkillSession, Source, SourceOptions, split_scheme};
use gage_store::{Store, StoreError};

use crate::error::CliError;

/// Long help shared by every `--native` option
pub const SOURCE_HELP: &str = "Session directory as SCHEME:PATH, SCHEME alone for the default \
directory of that scheme, or a name set under native in the config file. A name is looked up \
before a scheme. A path with no scheme is an error. Schemes are defined by drivers:

  claude  Claude Code sessions under \
PATH/projects; without PATH, the
          Claude Code home directory
  codex   Codex sessions under PATH/sessions; without PATH, the
          Codex home directory
  pi      Pi sessions under PATH/sessions, or under PATH when it has
          no sessions directory; without PATH, the Pi agent directory";

/// The drivers this CLI build registers. The default is the one the
/// config names, once [`configure_drivers`] has run.
pub fn driver_registry() -> DriverRegistry {
    DriverRegistry::builtin()
}

/// Configure the process's drivers from the `drivers` and `native`
/// settings of the user config and the `native` settings of the
/// project configs: the default driver, the models each driver
/// resolves its sizes to, and the names of native sources, of which
/// `default` is the source a command reads when it is given none. A
/// config that cannot be read, that names a default driver this build
/// does not have, or that gives a name to something that is not a
/// source, is the error.
pub fn configure_drivers() -> Result<(), CliError> {
    let path = display_user_config_path();
    let config = Config::load_user().with_context(|| format!("reading {path}"))?;
    set_driver_models(driver_models(&config.drivers))
        .with_context(|| format!("{path}: drivers"))?;
    set_default_driver(&config.drivers.default)
        .with_context(|| format!("{path}: drivers.default"))?;
    let native = layered_native(&path, &config.native, project_native()?)?;
    set_native_names(native.into_iter().collect())
        .context("native")
        .map_err(CliError::from)
}

/// The `[native]` table of one project config
pub struct ProjectNative {
    /// The config file
    pub path: PathBuf,
    /// Each name with its source. A relative path is under the
    /// project directory, which is the one that holds `.gage`.
    pub sources: BTreeMap<String, String>,
}

/// The native sources of each project config found walking up from
/// the current directory, outermost first, so a later project
/// replaces the names of an earlier one.
pub fn project_native() -> anyhow::Result<Vec<ProjectNative>> {
    let cwd = env::current_dir().context("reading the current directory")?;
    project_native_in(&cwd)
}

fn project_native_in(start: &Path) -> anyhow::Result<Vec<ProjectNative>> {
    let mut projects = Vec::new();
    for path in project_config_paths(start).into_iter().rev() {
        let shown = shorten_home(&path);
        let config = ProjectConfig::load_from(&path).with_context(|| format!("reading {shown}"))?;
        let project_dir = path
            .parent()
            .and_then(Path::parent)
            .expect("a project config should be at <project>/.gage/config.toml");
        let mut sources = BTreeMap::new();
        for (name, source) in config.native {
            let source = anchor_source(&source, project_dir)
                .map_err(|e| anyhow!("{shown}: native.{name}: {e}"))?;
            sources.insert(name, source);
        }
        projects.push(ProjectNative { path, sources });
    }
    Ok(projects)
}

/// `source` with a relative path placed under `base`. A scheme alone
/// and a path that is absolute or under `~` are returned as written.
fn anchor_source(source: &str, base: &Path) -> Result<String, String> {
    let Some(path) = relative_path(source) else {
        return Ok(source.to_string());
    };
    let scheme = &source[..source.len() - path.len()];
    let anchored = base.join(path);
    match anchored.to_str() {
        Some(anchored) => Ok(format!("{scheme}{anchored}")),
        None => Err(format!(
            "{} is not UTF-8, so {source:?} cannot be written under it",
            base.display()
        )),
    }
}

/// The path of `source`, when it has one that is neither absolute nor
/// under `~`
fn relative_path(source: &str) -> Option<&str> {
    let (_, path) = split_scheme(source)?;
    let fixed = path.is_empty() || path == "~" || path.starts_with("~/");
    (!fixed && Path::new(path).is_relative()).then_some(path)
}

/// The native sources in effect: those of the user config at
/// `user_path`, with each name a project sets replaced by the
/// project's. Each config is checked alone, so an error names the
/// file at fault. A relative path in the user config is an error,
/// since no project directory places it.
fn layered_native(
    user_path: &str,
    user: &BTreeMap<String, String>,
    projects: Vec<ProjectNative>,
) -> anyhow::Result<BTreeMap<String, String>> {
    for (name, source) in user {
        check_user_path(name, source).with_context(|| user_path.to_string())?;
    }
    check_native(user).with_context(|| user_path.to_string())?;
    let mut native = user.clone();
    for project in projects {
        check_native(&project.sources).with_context(|| shorten_home(&project.path))?;
        native.extend(project.sources);
    }
    Ok(native)
}

/// Check that `source`, which the user config gives `name`, has no
/// relative path.
fn check_user_path(name: &str, source: &str) -> anyhow::Result<()> {
    if relative_path(source).is_some() {
        bail!(
            "native.{name}: {source:?} has a relative path; write an absolute path or a path \
             under ~"
        );
    }
    Ok(())
}

/// Check one entry of a `[native]` table as a config load checks it.
/// `project_dir` is the directory of the project whose config holds
/// the entry, and `None` when the user config holds it. The error is
/// `native.<name>: <reason>`.
pub fn check_native_entry(
    name: &str,
    source: &str,
    project_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let source = match project_dir {
        Some(dir) => anchor_source(source, dir).map_err(|e| anyhow!("native.{name}: {e}"))?,
        None => {
            check_user_path(name, source)?;
            source.to_string()
        }
    };
    check_native(&BTreeMap::from([(name.to_string(), source)]))
}

/// Check the names and sources of one `[native]` table as the
/// registry reads them.
fn check_native(native: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let names = native
        .iter()
        .map(|(name, source)| (name.clone(), source.clone()))
        .collect();
    driver_registry().name_sources(names)?;
    Ok(())
}

/// The model sizes `config` sets, by driver name
pub fn driver_models(config: &DriversConfig) -> HashMap<String, ModelSizes> {
    config
        .entries
        .iter()
        .map(|(name, entry)| {
            let models = &entry.models;
            let sizes = ModelSizes {
                small: models.small.clone(),
                medium: models.medium.clone(),
                large: models.large.clone(),
            };
            (name.clone(), sizes)
        })
        .collect()
}

/// Resolve a `--native` value to the driver that handles it and the
/// value to hand that driver. `None` is the empty source, which is
/// the default native source the config sets, and the default
/// driver's default location when it sets none. A name the config
/// gives a
/// source is that source. A driver name alone is that driver's
/// default location. A value that names no driver is the
/// error `--native <value>: <reason>`.
pub fn resolve_source(
    registry: &DriverRegistry,
    source: Option<&str>,
) -> Result<(Arc<dyn Driver>, String), String> {
    let source = source.unwrap_or("");
    registry.resolve_source(source).map_err(|e| {
        if source.is_empty() {
            e.to_string()
        } else {
            format!("--native {source}: {e}")
        }
    })
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

/// Long help shared by every `--stored-remote` option, after the
/// option's own short line
pub const STORED_REMOTE_HELP: &str = "NAME is a remote that gage fetch brought objects from. Those objects are kept apart from the store until gage merge merges them. Nothing is fetched and nothing is written";

/// Open the store a command reads: its own objects, or with
/// `stored_remote` the objects fetched from that remote, which are
/// read and never written.
pub fn open_stored(stored_remote: Option<&str>) -> Result<Store, StoreError> {
    let path = gage_store::store_path();
    match stored_remote {
        None => Store::open(&path),
        Some(remote) => Store::open_stored_remote(&path, &default_cache_root(), remote),
    }
}

/// The error of a command that writes sessions and was given
/// `--stored-remote`: `<command>: --stored-remote does not apply; ...`.
pub fn stored_remote_is_read_only(command: &str) -> CliError {
    CliError::message(format!(
        "{command}: --stored-remote does not apply; sessions fetched from a remote are read only"
    ))
}

/// The default driver, or `command: <error>`
pub fn default_driver(command: &str) -> Result<Arc<dyn Driver>, CliError> {
    driver_registry().default().ok_or_else(|| {
        CliError::message(format!("{command}: no default session driver registered"))
    })
}

/// Open `source` through the registry. `label` names the caller in the
/// error: `<label>: <source>: <error>`.
pub fn open_source(source: &str, label: &str) -> Result<Box<dyn Source>, CliError> {
    driver_registry()
        .open_source(&SourceOptions::new(source))
        .with_context(|| format!("{label}: {source}"))
        .map_err(CliError::from)
}

/// Start `session` in the default driver's harness and wait for it to
/// end. The harness owns the terminal until then. A non-zero exit is
/// the error `<command>: session exited with status <code>`.
pub fn run_skill_session(command: &str, session: &SkillSession<'_>) -> Result<(), CliError> {
    let driver = default_driver(command)?;
    let mut running = driver
        .start_skill_session(session)
        .context(command.to_string())?;
    let code = running
        .wait()
        .with_context(|| format!("{command}: waiting for the session"))?;
    if code != 0 {
        return Err(CliError::message(format!(
            "{command}: session exited with status {code}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn resolved(source: Option<&str>) -> Result<(&'static str, String), String> {
        resolve_source(&driver_registry(), source).map(|(driver, spec)| (driver.name(), spec))
    }

    #[test]
    fn a_driver_name_alone_is_its_default_location() {
        assert_eq!(resolved(Some("codex")), Ok(("codex", "codex:".to_string())));
        assert_eq!(resolved(Some("pi:")), Ok(("pi", "pi:".to_string())));
        assert_eq!(
            resolved(Some("claude:/tmp/x")),
            Ok(("claude", "claude:/tmp/x".to_string()))
        );
        assert_eq!(resolved(None).unwrap().1, "");
    }

    fn table(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, source)| (name.to_string(), source.to_string()))
            .collect()
    }

    #[test]
    fn a_relative_path_in_a_project_config_is_under_the_project_directory() {
        let base = Path::new("/proj");
        let anchored = |source: &str| anchor_source(source, base).unwrap();
        assert_eq!(anchored("pi:sessions"), "pi:/proj/sessions");
        assert_eq!(anchored("claude:./a/b"), "claude:/proj/./a/b");
        for fixed in ["pi", "pi:", "pi:/abs", "pi:~", "pi:~/x"] {
            assert_eq!(anchored(fixed), fixed);
        }
    }

    #[test]
    fn project_native_sources_are_read_outermost_first() {
        let root = tempfile::tempdir().unwrap();
        let inner = root.path().join("outer/inner");
        fs::create_dir_all(inner.join(".gage")).unwrap();
        fs::create_dir_all(root.path().join("outer/.gage")).unwrap();
        fs::write(
            root.path().join("outer/.gage/config.toml"),
            "[native]\ndefault = \"pi:sessions\"\nwork = \"codex:/w\"\n",
        )
        .unwrap();
        fs::write(
            inner.join(".gage/config.toml"),
            "[native]\ndefault = \"claude:logs\"\n",
        )
        .unwrap();
        let projects: Vec<ProjectNative> = project_native_in(&inner)
            .unwrap()
            .into_iter()
            .filter(|project| project.path.starts_with(root.path()))
            .collect();
        let outer = root.path().join("outer");
        assert_eq!(projects.len(), 2);
        assert_eq!(
            projects[0].sources,
            table(&[
                (
                    "default",
                    &format!("pi:{}", outer.join("sessions").display())
                ),
                ("work", "codex:/w"),
            ])
        );
        assert_eq!(
            projects[1].sources,
            table(&[(
                "default",
                &format!("claude:{}", inner.join("logs").display())
            )])
        );
    }

    #[test]
    fn a_project_replaces_the_names_it_sets_and_keeps_the_rest() {
        let user = table(&[("default", "claude"), ("work", "pi:~/w"), ("old", "codex")]);
        let projects = vec![
            ProjectNative {
                path: PathBuf::from("/a/.gage/config.toml"),
                sources: table(&[("default", "pi:/a/s"), ("work", "pi:/a/w")]),
            },
            ProjectNative {
                path: PathBuf::from("/a/b/.gage/config.toml"),
                sources: table(&[("default", "codex:/a/b/s")]),
            },
        ];
        assert_eq!(
            layered_native("user.toml", &user, projects).unwrap(),
            table(&[
                ("default", "codex:/a/b/s"),
                ("old", "codex"),
                ("work", "pi:/a/w"),
            ])
        );
    }

    #[test]
    fn a_native_table_that_breaks_the_rules_names_its_file() {
        let err = |user: &[(&str, &str)], project: &[(&str, &str)]| {
            let projects = vec![ProjectNative {
                path: PathBuf::from("/a/.gage/config.toml"),
                sources: table(project),
            }];
            let err = layered_native("user.toml", &table(user), projects)
                .err()
                .unwrap();
            format!("{err:#}")
        };
        assert_eq!(
            err(&[("default", "pi:sessions")], &[]),
            "user.toml: native.default: \"pi:sessions\" has a relative path; write an \
             absolute path or a path under ~"
        );
        assert_eq!(
            err(&[("default", "zzz:/x")], &[]),
            "user.toml: native.default: no driver serves scheme \"zzz\"; schemes: claude, \
             codex, pi"
        );
        assert_eq!(
            err(&[("work", "pi:/w")], &[("default", "work")]),
            "/a/.gage/config.toml: native.default: \"work\" is not a source; write it as \
             SCHEME:PATH or SCHEME, where SCHEME is one of claude, codex, pi"
        );
    }

    #[test]
    fn a_value_that_names_no_driver_is_an_error_naming_the_value() {
        assert_eq!(
            resolved(Some("/tmp/x")).unwrap_err(),
            "--native /tmp/x: source names no driver; write it as SCHEME:PATH, \
             where SCHEME is one of claude, codex, pi, or as a name set under native in \
             the config"
        );
        assert_eq!(
            resolved(Some("zzz:/x")).unwrap_err(),
            "--native zzz:/x: no driver serves scheme \"zzz\"; schemes: claude, codex, pi"
        );
    }
}
