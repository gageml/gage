//! Pre-flight checks against the user's `claude` CLI.
//!
//! Both checks shell out to `claude` and parse `--json` output; neither
//! spawns a session or consumes tokens. They are cheap enough to run at
//! the top of every command that depends on the Gage plugin.

use std::io;
use std::process::{Command, Output};

use serde::Deserialize;

use crate::proc::find_claude;

/// Plugin id as installed by `gage init` (see [`crate::plugin`]) and
/// reported by `claude plugin list --json`.
pub const PLUGIN_ID: &str = "gage@gage";

/// Plugin version this build of `gage` was compiled with. Sourced from
/// the workspace `Cargo.toml` and substituted into the plugin's
/// `plugin.json` at install time (see [`crate::plugin`]), so a fresh
/// `gage init` after any workspace version bump brings the installed
/// plugin into line with this constant.
pub const EXPECTED_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub enum PreflightError {
    /// `claude auth status` reported `loggedIn: false`.
    Auth,
    /// `claude plugin list` did not include [`PLUGIN_ID`].
    PluginMissing,
    /// The installed plugin's version differs from [`EXPECTED_VERSION`].
    PluginVersionMismatch { installed: String, expected: String },
    /// Any other failure: `claude` not on PATH, non-zero exit, or
    /// output that did not parse. The message is safe to render to the
    /// user verbatim.
    Other(String),
}

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth => write!(
                f,
                "Not logged in to Claude Code. Run `claude` and complete /login."
            ),
            Self::PluginMissing => write!(
                f,
                "Gage plugin is not installed. Run `gage init` to install it."
            ),
            Self::PluginVersionMismatch {
                installed,
                expected,
            } => write!(
                f,
                "Gage plugin version {installed} is installed, but this build \
                 of gage expects {expected}. Run `gage init` to reinstall.",
            ),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PreflightError {}

/// Run both checks. Returns the first failure; a `plugin` failure is
/// reported before an `auth` failure.
pub fn check() -> Result<(), PreflightError> {
    check_plugin()?;
    check_auth()?;
    Ok(())
}

/// One entry from `claude plugin list --json`. Extra fields the CLI
/// may add are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct InstalledPlugin {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "installPath", default)]
    pub install_path: Option<String>,
}

/// Return the entry for [`PLUGIN_ID`] from `claude plugin list --json`,
/// or `None` if the plugin is not installed. Does not check the version;
/// the caller compares against [`EXPECTED_VERSION`] if desired.
pub fn installed_plugin() -> Result<Option<InstalledPlugin>, PreflightError> {
    let output = run_claude(&["plugin", "list", "--json"])?;
    let entries: Vec<InstalledPlugin> = serde_json::from_slice(&output.stdout).map_err(|e| {
        PreflightError::Other(format!(
            "Could not parse `claude plugin list --json` output: {e}"
        ))
    })?;
    Ok(entries.into_iter().find(|e| e.id == PLUGIN_ID))
}

/// Verify the Gage plugin is installed and its version matches
/// [`EXPECTED_VERSION`].
pub fn check_plugin() -> Result<(), PreflightError> {
    match installed_plugin()? {
        None => Err(PreflightError::PluginMissing),
        Some(p) if p.version != EXPECTED_VERSION => Err(PreflightError::PluginVersionMismatch {
            installed: p.version,
            expected: EXPECTED_VERSION.to_string(),
        }),
        Some(_) => Ok(()),
    }
}

/// Parsed `claude auth status --json`. Only the fields the status
/// command needs are named; the CLI is free to add more.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthStatus {
    #[serde(rename = "loggedIn")]
    pub logged_in: bool,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(rename = "authMethod", default)]
    pub auth_method: Option<String>,
    #[serde(rename = "apiProvider", default)]
    pub api_provider: Option<String>,
    #[serde(rename = "orgName", default)]
    pub org_name: Option<String>,
    #[serde(rename = "subscriptionType", default)]
    pub subscription_type: Option<String>,
}

/// Return the parsed output of `claude auth status --json`.
pub fn auth_status() -> Result<AuthStatus, PreflightError> {
    let output = run_claude(&["auth", "status", "--json"])?;
    serde_json::from_slice(&output.stdout).map_err(|e| {
        PreflightError::Other(format!(
            "Could not parse `claude auth status --json` output: {e}"
        ))
    })
}

/// Verify the user is logged in to Claude.
pub fn check_auth() -> Result<(), PreflightError> {
    if auth_status()?.logged_in {
        Ok(())
    } else {
        Err(PreflightError::Auth)
    }
}

fn run_claude(args: &[&str]) -> Result<Output, PreflightError> {
    let bin = find_claude().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => PreflightError::Other(
            "The `claude` CLI was not found on PATH. Install Claude Code and try again."
                .to_string(),
        ),
        _ => PreflightError::Other(format!("Locating `claude` failed: {e}")),
    })?;
    let output = Command::new(&bin).args(args).output().map_err(|e| {
        PreflightError::Other(format!("Failed to run `claude {}`: {e}", args.join(" ")))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(PreflightError::Other(format!(
            "`claude {}` exited with {}: {stderr}",
            args.join(" "),
            output.status,
        )));
    }
    Ok(output)
}
