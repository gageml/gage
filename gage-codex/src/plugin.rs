//! The Gage plugin in Codex: how it is rendered for Codex, where it
//! is rendered, and what `codex plugin` says about the install.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gage_core::config::gage_home;
use gage_plugin::{PLUGIN_ID, PluginTarget, VERSION};
use gage_session::{DriverError, Field, InstallInfo};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::driver::NAME;
use crate::harness::HARNESS_NAME;

/// What Codex puts in front of an MCP tool's name: `mcp__<server>__`
pub(crate) const TOOL_PREFIX: &str = "mcp__gage__";

/// What invokes a skill in a Codex prompt
pub(crate) const SKILL_SIGIL: &str = "$";

/// The Gage variables the MCP server needs from the environment
/// `gage init` ran in. Codex starts an MCP server with a short fixed
/// list of variables, so the ones that move Gage's home and store are
/// written into the server's registration.
const FORWARDED_VARS: &[&str] = &["GAGE_HOME", "GAGE_STORE"];

/// The plugin for Codex. Codex runs none of the plugin's sub-agents,
/// so the skills do those steps in the main session.
pub(crate) fn target() -> PluginTarget<'static> {
    PluginTarget {
        agent: HARNESS_NAME,
        tool_prefix: TOOL_PREFIX,
        source_scheme: NAME,
        skill_sigil: SKILL_SIGIL,
        subagents: None,
        plugin_manifest: ".codex-plugin/plugin.json",
        marketplace_manifest: ".agents/plugins/marketplace.json",
    }
}

/// Where the plugin is rendered for Codex to install from:
/// `<gage_home>/.plugin-marketplace-codex`. The directory is the
/// marketplace root and the plugin root.
pub(crate) fn plugin_dir() -> PathBuf {
    gage_home().join(".plugin-marketplace-codex")
}

/// The members of the MCP server's registration beyond its command.
/// The approval mode is what lets a headless Codex run call Gage's
/// tools: `codex exec` never asks, and it refuses a tool that is not
/// annotated read-only unless the server's own registration approves
/// its tools. A setting on the user's side can only narrow that.
pub(crate) fn mcp_settings() -> Map<String, Value> {
    mcp_settings_with(|name| env::var_os(name))
}

fn mcp_settings_with(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Map<String, Value> {
    let mut settings = Map::new();
    settings.insert("default_tools_approval_mode".to_string(), json!("approve"));
    let forwarded: Map<String, Value> = FORWARDED_VARS
        .iter()
        .filter_map(|name| {
            let value = var(name).filter(|v| !v.is_empty())?;
            Some((name.to_string(), json!(value.to_string_lossy())))
        })
        .collect();
    if !forwarded.is_empty() {
        settings.insert("env".to_string(), Value::Object(forwarded));
    }
    settings
}

/// One installed plugin as `codex plugin list --json` reports it.
/// Members Codex adds are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstalledPlugin {
    plugin_id: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    source: Option<PluginSource>,
}

#[derive(Debug, Clone, Deserialize)]
struct PluginSource {
    #[serde(default)]
    path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct PluginList {
    #[serde(default)]
    installed: Vec<InstalledPlugin>,
}

/// The Gage plugin's entry in `codex plugin list --json`, or `None`
/// when it is not installed.
pub(crate) fn installed_plugin(codex: &Path) -> Result<Option<InstalledPlugin>, String> {
    let output = run(codex, &["plugin", "list", "--json"])?;
    parse_plugin_list(&output.stdout)
}

fn parse_plugin_list(stdout: &[u8]) -> Result<Option<InstalledPlugin>, String> {
    let list: PluginList = serde_json::from_slice(stdout)
        .map_err(|e| format!("Could not parse `codex plugin list --json` output: {e}"))?;
    Ok(list
        .installed
        .into_iter()
        .find(|p| p.plugin_id == PLUGIN_ID))
}

/// Why a skill session cannot start, worded for the user: the plugin
/// is missing or is not the version this build installs.
pub(crate) fn check_plugin(codex: &Path) -> Result<(), String> {
    plugin_check(installed_plugin(codex)?)
}

fn plugin_check(installed: Option<InstalledPlugin>) -> Result<(), String> {
    match installed {
        None => Err("Gage plugin is not installed in Codex. Run `gage init` to install it.".into()),
        Some(p) if p.version.as_deref() != Some(VERSION) => Err(format!(
            "Gage plugin version {} is installed in Codex, but this build of gage expects \
             {VERSION}. Run `gage init` to reinstall.",
            p.version.as_deref().unwrap_or("unknown"),
        )),
        Some(_) => Ok(()),
    }
}

/// Gage's installation in the Codex at `codex`.
pub(crate) fn install_status(codex: &Path) -> Field<InstallInfo> {
    match installed_plugin(codex) {
        Ok(installed) => install_field(installed),
        Err(e) => Field::Problem {
            info: None,
            message: format!("plugin status unavailable: {e}"),
            remedy: String::new(),
        },
    }
}

fn install_field(installed: Option<InstalledPlugin>) -> Field<InstallInfo> {
    let expected_version = VERSION.to_string();
    let Some(plugin) = installed else {
        return Field::Problem {
            info: Some(InstallInfo {
                installed: false,
                version: None,
                expected_version,
                enabled: false,
                path: None,
            }),
            message: "Gage plugin not installed".to_string(),
            remedy: "run `gage init`".to_string(),
        };
    };
    let info = InstallInfo {
        installed: true,
        version: plugin.version.clone(),
        expected_version: expected_version.clone(),
        enabled: plugin.enabled,
        path: plugin.source.and_then(|s| s.path),
    };
    if plugin.version.as_deref() == Some(VERSION) {
        return Field::Ok(info);
    }
    Field::Problem {
        info: Some(info),
        message: format!(
            "Gage plugin v{} installed, this gage expects v{expected_version}",
            plugin.version.as_deref().unwrap_or("unknown")
        ),
        remedy: "run `gage init`".to_string(),
    }
}

/// Run `codex` with `args`; a failure to start it or a non-zero exit
/// is the error, with what the command printed.
pub(crate) fn run_strict(codex: &Path, args: &[&OsStr]) -> Result<(), DriverError> {
    let output = Command::new(codex)
        .args(args)
        .output()
        .map_err(DriverError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    Err(DriverError::Other(exit_message(args, &output)))
}

/// Run `codex` with `args`. A failure to start `codex` is the error;
/// a non-zero exit is the inner error, what the command said, for a
/// caller to whom the exit can mean the thing was not there to
/// remove.
pub(crate) fn run_lenient(
    codex: &Path,
    args: &[&OsStr],
) -> Result<Result<(), String>, DriverError> {
    let output = Command::new(codex)
        .args(args)
        .output()
        .map_err(DriverError::Io)?;
    if output.status.success() {
        return Ok(Ok(()));
    }
    Ok(Err(exit_message(args, &output)))
}

fn run(codex: &Path, args: &[&str]) -> Result<Output, String> {
    let output = Command::new(codex)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to run `codex {}`: {e}", args.join(" ")))?;
    if !output.status.success() {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        return Err(exit_message(&args, &output));
    }
    Ok(output)
}

/// What a failed `codex` command said. Codex reports some failures on
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
        "`codex {}` failed ({}): {detail}",
        args.join(" "),
        output.status
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use gage_plugin::write_plugin;

    use super::*;

    #[test]
    fn the_plugin_is_written_in_codexs_layout_without_subagents() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), &target()).unwrap();

        let manifest =
            fs::read_to_string(dir.path().join(".codex-plugin").join("plugin.json")).unwrap();
        assert!(manifest.contains(&format!("\"version\": \"{VERSION}\"")));
        assert!(
            dir.path()
                .join(".agents")
                .join("plugins")
                .join("marketplace.json")
                .exists()
        );
        assert!(!dir.path().join("agents").exists());
        let resolve =
            fs::read_to_string(dir.path().join("skills").join("resolve").join("SKILL.md")).unwrap();
        assert!(resolve.contains("`mcp__gage__issue_assign`"));
        assert!(!resolve.contains("subagent_type"));
    }

    #[test]
    fn the_server_registration_approves_its_tools() {
        let settings = mcp_settings_with(|_| None);
        assert_eq!(settings["default_tools_approval_mode"], "approve");
        assert!(!settings.contains_key("env"));
    }

    #[test]
    fn gage_home_and_store_are_forwarded_to_the_server_when_set() {
        let settings = mcp_settings_with(|name| match name {
            "GAGE_HOME" => Some(OsString::from("/tmp/gh")),
            "GAGE_STORE" => Some(OsString::new()),
            _ => None,
        });
        assert_eq!(settings["env"], json!({ "GAGE_HOME": "/tmp/gh" }));
    }

    const LIST: &str = r#"{
      "installed": [
        {"pluginId": "other@debug", "version": "1.0.0", "installed": true, "enabled": true},
        {"pluginId": "gage@gage", "name": "gage", "marketplaceName": "gage",
         "version": "0.0.1", "installed": true, "enabled": true,
         "source": {"source": "local", "path": "/home/u/.gage/.plugin-marketplace-codex"}}
      ],
      "available": []
    }"#;

    #[test]
    fn the_gage_plugin_is_found_in_the_plugin_list() {
        let plugin = parse_plugin_list(LIST.as_bytes()).unwrap().unwrap();
        assert_eq!(plugin.version.as_deref(), Some("0.0.1"));
        assert!(plugin.enabled);
        assert_eq!(
            plugin.source.unwrap().path.unwrap(),
            Path::new("/home/u/.gage/.plugin-marketplace-codex")
        );
        assert!(
            parse_plugin_list(br#"{"installed": []}"#)
                .unwrap()
                .is_none()
        );
        assert!(parse_plugin_list(b"not json").is_err());
    }

    #[test]
    fn a_stale_or_missing_plugin_is_a_problem_with_the_init_remedy() {
        let stale = parse_plugin_list(LIST.as_bytes()).unwrap();
        let (message, remedy) = match install_field(stale.clone()) {
            Field::Problem {
                message, remedy, ..
            } => (message, remedy),
            other => panic!("{other:?}"),
        };
        assert!(message.contains("v0.0.1"), "{message}");
        assert_eq!(remedy, "run `gage init`");
        assert!(plugin_check(stale).unwrap_err().contains("0.0.1"));

        assert!(install_field(None).problem().is_some());
        assert!(plugin_check(None).unwrap_err().contains("not installed"));
    }

    #[test]
    fn a_plugin_at_this_builds_version_is_ok() {
        let list = LIST.replace("0.0.1", VERSION);
        let current = parse_plugin_list(list.as_bytes()).unwrap();
        assert!(matches!(install_field(current.clone()), Field::Ok(_)));
        assert!(plugin_check(current).is_ok());
    }
}
