//! Gage in Pi: how the plugin is rendered for Pi, where it is
//! rendered, how it is installed, and what Pi's own files say about
//! the install.
//!
//! Pi takes Gage in two parts. The skills are a Pi package: a
//! directory with a `package.json` and a `skills` directory, which
//! `pi install` records in Pi's settings and loads from where it is.
//! The MCP server is a registration in Pi's `mcp.json`, which
//! `pi mcp add` writes. A package cannot carry a server registration.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gage_core::config::gage_home;
use gage_plugin::{MCP_SERVER_NAME, PluginTarget, SkillNames, VERSION};
use gage_session::{DriverError, Field, InstallInfo};
use serde_json::Value;

use crate::driver::NAME;
use crate::harness::HARNESS_NAME;
use crate::home::agent_dir;

/// What Pi puts in front of an MCP tool's name: `mcp__<server>__`
pub(crate) const TOOL_PREFIX: &str = "mcp__gage__";

/// The file of a Pi package that names it and its version
const PACKAGE_MANIFEST: &str = "package.json";

/// What Pi shows the model about the Gage MCP server
const SERVER_DESCRIPTION: &str =
    "Query coding agent sessions and the notes and issues written about them";

/// The plugin for Pi. Pi keeps every skill under one set of names, so
/// each skill carries the plugin's name in its own. Pi runs none of
/// the plugin's sub-agents, so the skills do those steps in the main
/// session.
pub(crate) fn target() -> PluginTarget<'static> {
    PluginTarget {
        agent: HARNESS_NAME,
        tool_prefix: TOOL_PREFIX,
        source_scheme: NAME,
        skill_names: SkillNames::Global,
        skill_invoke: "/skill:",
        subagents: None,
        plugin_manifest: PACKAGE_MANIFEST,
        marketplace_manifest: None,
    }
}

/// Where the plugin is rendered for Pi to load: the Pi package
/// `<gage_home>/.plugin-package-pi`. Pi loads a local package from
/// where it is, so the directory stays for as long as Gage is
/// installed in Pi.
pub(crate) fn plugin_dir() -> PathBuf {
    gage_home().join(".plugin-package-pi")
}

/// The arguments of the `pi` command that registers the Gage MCP
/// server with the launch command `mcp_cmd`: the executable, then its
/// arguments. The tools are declared to the model as Pi's own are.
/// Pi's default keeps an MCP server's tools out of the model's sight,
/// to be found and called from a script, which suits a large server
/// and costs a small one its use.
pub(crate) fn mcp_add_args<'a>(mcp_cmd: &[&'a str]) -> Vec<&'a OsStr> {
    const REGISTER: [&str; 8] = [
        "mcp",
        "add",
        MCP_SERVER_NAME,
        "--exposure",
        "direct",
        "--description",
        SERVER_DESCRIPTION,
        "--",
    ];
    REGISTER
        .iter()
        .chain(mcp_cmd)
        .map(|arg| OsStr::new(*arg))
        .collect()
}

/// The arguments of the `pi` command that removes the Gage MCP
/// server's registration
pub(crate) fn mcp_remove_args() -> [&'static OsStr; 3] {
    ["mcp", "remove", MCP_SERVER_NAME].map(OsStr::new)
}

/// What Pi's own files say of Gage's install
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Installed {
    /// Pi's settings list the package
    package: bool,
    /// The version the package's manifest names
    version: Option<String>,
    /// The MCP server's registration: whether Pi connects to it.
    /// `None` when the server is not registered.
    server: Option<bool>,
    /// Where the package is
    path: PathBuf,
}

/// Gage's install in the default Pi agent directory, read from the
/// files Pi keeps there. `pi mcp list` is not asked: it connects to
/// every server the user has registered.
pub(crate) fn installed() -> Result<Installed, String> {
    let agent_dir = agent_dir().ok_or("PI_CODING_AGENT_DIR or HOME must be set")?;
    installed_in(&agent_dir, &plugin_dir())
}

fn installed_in(agent_dir: &Path, plugin_dir: &Path) -> Result<Installed, String> {
    let settings = read_json(&agent_dir.join("settings.json"))?;
    let servers = read_json(&agent_dir.join("mcp.json"))?;
    let manifest = read_json(&plugin_dir.join(PACKAGE_MANIFEST))?;
    let package = settings
        .as_ref()
        .and_then(|settings| settings.get("packages")?.as_array())
        .is_some_and(|packages| {
            packages
                .iter()
                .filter_map(package_source)
                .any(|source| same_dir(&agent_dir.join(source), plugin_dir))
        });
    let server = servers
        .as_ref()
        .and_then(|servers| servers.get("mcpServers")?.get(MCP_SERVER_NAME))
        .map(|entry| entry.get("enabled").and_then(Value::as_bool) != Some(false));
    let version = manifest
        .as_ref()
        .and_then(|manifest| manifest.get("version")?.as_str())
        .map(String::from);
    Ok(Installed {
        package,
        version,
        server,
        path: plugin_dir.to_path_buf(),
    })
}

/// The source of one entry of the settings' `packages`: the entry
/// itself, or its `source` when the entry also selects resources.
fn package_source(entry: &Value) -> Option<&str> {
    match entry {
        Value::String(source) => Some(source),
        other => other.get("source")?.as_str(),
    }
}

/// Whether two paths name one directory. Pi writes a local package's
/// source relative to its settings file, so the paths are compared as
/// the filesystem resolves them; one that does not resolve, a package
/// directory since removed among them, is compared as written.
fn same_dir(a: &Path, b: &Path) -> bool {
    let resolved = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved(a) == resolved(b)
}

/// The JSON document at `path`; `None` when there is no such file.
fn read_json(path: &Path) -> Result<Option<Value>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("reading {}: {e}", path.display()))
}

/// Why a skill session cannot start, worded for the user: Gage is not
/// installed in Pi, or is not the version this build installs.
pub(crate) fn check_plugin() -> Result<(), String> {
    plugin_check(&installed()?)
}

fn plugin_check(installed: &Installed) -> Result<(), String> {
    if !installed.package || installed.server.is_none() {
        return Err("Gage is not installed in Pi. Run `gage init` to install it.".into());
    }
    if installed.version.as_deref() != Some(VERSION) {
        return Err(format!(
            "Gage plugin version {} is installed in Pi, but this build of gage expects \
             {VERSION}. Run `gage init` to reinstall.",
            installed.version.as_deref().unwrap_or("unknown"),
        ));
    }
    Ok(())
}

/// Gage's installation in Pi.
pub(crate) fn install_status() -> Field<InstallInfo> {
    match installed() {
        Ok(installed) => install_field(installed),
        Err(e) => Field::Problem {
            info: None,
            message: format!("plugin status unavailable: {e}"),
            remedy: String::new(),
        },
    }
}

fn install_field(installed: Installed) -> Field<InstallInfo> {
    let expected_version = VERSION.to_string();
    let info = InstallInfo {
        installed: installed.package && installed.server.is_some(),
        version: installed.version.clone().filter(|_| installed.package),
        expected_version: expected_version.clone(),
        enabled: installed.server == Some(true),
        path: installed.package.then(|| installed.path.clone()),
    };
    let problem = |message: String| Field::Problem {
        info: Some(info.clone()),
        message,
        remedy: "run `gage init`".to_string(),
    };
    if !installed.package && installed.server.is_none() {
        return problem("Gage plugin not installed".to_string());
    }
    if !installed.package {
        return problem("Gage skills not installed".to_string());
    }
    if installed.server.is_none() {
        return problem("Gage MCP server not registered".to_string());
    }
    if installed.version.as_deref() != Some(VERSION) {
        return problem(format!(
            "Gage plugin v{} installed, this gage expects v{expected_version}",
            installed.version.as_deref().unwrap_or("unknown")
        ));
    }
    Field::Ok(info)
}

/// Run `pi` with `args`; a failure to start it or a non-zero exit is
/// the error, with what the command printed.
pub(crate) fn run_strict(pi: &Path, args: &[&OsStr]) -> Result<(), DriverError> {
    let output = command(pi, args).output().map_err(DriverError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(DriverError::Other(exit_message(args, &output)))
}

/// Run `pi` with `args`. A failure to start `pi` is the error; a
/// non-zero exit is the inner error, what the command said, for a
/// caller to whom the exit can mean the thing was not there to
/// remove.
pub(crate) fn run_lenient(pi: &Path, args: &[&OsStr]) -> Result<Result<(), String>, DriverError> {
    let output = command(pi, args).output().map_err(DriverError::Io)?;
    if output.status.success() {
        return Ok(Ok(()));
    }
    Ok(Err(exit_message(args, &output)))
}

/// `pi` with `args`, run in Gage's home. Pi reads the settings and
/// the MCP registrations of the project it is run in, and a command
/// that installs for the user has no project.
fn command(pi: &Path, args: &[&OsStr]) -> Command {
    let mut cmd = Command::new(pi);
    cmd.args(args);
    let home = gage_home();
    if home.is_dir() {
        cmd.current_dir(home);
    }
    cmd
}

/// What a failed `pi` command said. Pi reports some failures on
/// stdout, so stdout is the detail when stderr has nothing.
fn exit_message(args: &[&OsStr], output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let args: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
    format!(
        "`pi {}` failed ({}): {detail}",
        args.join(" "),
        output.status
    )
}

#[cfg(test)]
mod tests {
    use gage_plugin::write_plugin;
    use serde_json::json;

    use super::*;

    #[test]
    fn the_plugin_is_written_as_a_pi_package_of_named_skills() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), &target()).unwrap();

        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(PACKAGE_MANIFEST)).unwrap())
                .unwrap();
        assert_eq!(manifest["name"], "gage");
        assert_eq!(manifest["version"], VERSION);
        assert!(!dir.path().join("agents").exists());
        let resolve = fs::read_to_string(
            dir.path()
                .join("skills")
                .join("gage-resolve")
                .join("SKILL.md"),
        )
        .unwrap();
        assert!(resolve.starts_with("---\nname: gage-resolve\n"));
        assert!(resolve.contains("`mcp__gage__issue_assign`"));
        assert!(!resolve.contains("subagent_type"));
        assert_eq!(target().skill_invocation("resolve"), "/skill:gage-resolve");
    }

    #[test]
    fn the_server_is_registered_with_its_tools_declared_to_the_model() {
        let args: Vec<&str> = mcp_add_args(&["/usr/local/bin/gage", "mcp"])
            .into_iter()
            .map(|a| a.to_str().unwrap())
            .collect();
        assert_eq!(args[..5], ["mcp", "add", "gage", "--exposure", "direct"]);
        assert_eq!(args[args.len() - 3..], ["--", "/usr/local/bin/gage", "mcp"]);
    }

    /// An agent directory and a package directory beside it, with the
    /// package written at this build's version
    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let package = tmp.path().join("package");
        fs::create_dir_all(&agent).unwrap();
        write_plugin(&package, &target()).unwrap();
        (tmp, agent, package)
    }

    fn write(path: &Path, value: Value) {
        fs::write(path, value.to_string()).unwrap();
    }

    #[test]
    fn an_install_is_read_from_the_settings_and_the_server_registrations() {
        let (_tmp, agent, package) = dirs();
        let none = installed_in(&agent, &package).unwrap();
        assert!(!none.package);
        assert_eq!(none.server, None);
        assert!(install_field(none.clone()).problem().is_some());
        assert!(plugin_check(&none).unwrap_err().contains("not installed"));

        // Pi writes a local package's source relative to its settings
        write(
            &agent.join("settings.json"),
            json!({"packages": ["npm:@example/other", "../package"]}),
        );
        let skills_only = installed_in(&agent, &package).unwrap();
        assert!(skills_only.package);
        let (message, remedy) = match install_field(skills_only) {
            Field::Problem {
                message, remedy, ..
            } => (message, remedy),
            other => panic!("{other:?}"),
        };
        assert_eq!(message, "Gage MCP server not registered");
        assert_eq!(remedy, "run `gage init`");

        write(
            &agent.join("mcp.json"),
            json!({"mcpServers": {"gage": {"command": "gage", "args": ["mcp"]}}}),
        );
        let both = installed_in(&agent, &package).unwrap();
        assert_eq!(both.server, Some(true));
        assert_eq!(both.version.as_deref(), Some(VERSION));
        assert!(plugin_check(&both).is_ok());
        match install_field(both) {
            Field::Ok(info) => {
                assert!(info.installed && info.enabled);
                assert_eq!(info.version.as_deref(), Some(VERSION));
                assert_eq!(info.path.as_deref(), Some(package.as_path()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_package_entry_that_selects_resources_names_its_source() {
        let (_tmp, agent, package) = dirs();
        write(
            &agent.join("settings.json"),
            json!({"packages": [{"source": package, "skills": []}]}),
        );
        assert!(installed_in(&agent, &package).unwrap().package);
    }

    #[test]
    fn a_stale_package_and_a_disabled_server_are_reported() {
        let (_tmp, agent, package) = dirs();
        write(&agent.join("settings.json"), json!({"packages": [package]}));
        write(
            &agent.join("mcp.json"),
            json!({"mcpServers": {"gage": {"command": "gage", "enabled": false}}}),
        );
        write(
            &package.join(PACKAGE_MANIFEST),
            json!({"name": "gage", "version": "0.0.1"}),
        );
        let stale = installed_in(&agent, &package).unwrap();
        assert_eq!(stale.server, Some(false));
        assert!(plugin_check(&stale).unwrap_err().contains("0.0.1"));
        match install_field(stale) {
            Field::Problem { info, message, .. } => {
                assert!(message.contains("v0.0.1"), "{message}");
                assert!(!info.unwrap().enabled);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_settings_file_that_is_not_json_is_an_error() {
        let (_tmp, agent, package) = dirs();
        fs::write(agent.join("settings.json"), "not json").unwrap();
        assert!(installed_in(&agent, &package).is_err());
    }
}
