//! The user's `pi` CLI: where it is, its version, and whether it can
//! reach a model. Nothing here starts a session or uses tokens.

use std::path::{Path, PathBuf};
use std::process::Command;

use gage_session::{AuthInfo, Field, HarnessInfo};
use serde::Deserialize;

/// The harness's display name
pub(crate) const HARNESS_NAME: &str = "Pi";

const NOT_FOUND: &str = "The `pi` CLI was not found on PATH. Install Pi and try again.";

/// The `pi` binary on PATH.
pub(crate) fn find_pi() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("pi"))
        .find(|candidate| candidate.is_file())
}

/// The `pi` binary on PATH, or why there is none, worded for the
/// user.
pub(crate) fn require_pi() -> Result<PathBuf, String> {
    find_pi().ok_or_else(|| NOT_FOUND.to_string())
}

/// The CLI's location and version. `Absent` when it is not installed.
pub(crate) fn harness_status() -> Field<HarnessInfo> {
    let Some(path) = find_pi() else {
        return Field::Absent {
            message: format!("{HARNESS_NAME} not installed"),
            remedy: format!("Install {HARNESS_NAME}"),
        };
    };
    let version = Command::new(&path)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| version_of(&String::from_utf8_lossy(&o.stdout)));
    Field::Ok(HarnessInfo {
        name: HARNESS_NAME.to_string(),
        version,
        path,
    })
}

/// The version in `pi --version` output, which is the version alone
fn version_of(said: &str) -> Option<String> {
    said.split_whitespace().last().map(String::from)
}

/// What `pi auth check --json` says about the credentials of one
/// model's provider. Members Pi adds are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthCheck {
    /// `ready`, `not_ready`, or `invalid`
    status: String,
    #[serde(default)]
    provider: Option<String>,
    /// How the provider is signed in to, `oauth` or `api_key`, when
    /// it is
    #[serde(default)]
    auth_type: Option<String>,
    /// Why the credentials are not ready, in Pi's code for it
    #[serde(default)]
    reason: Option<String>,
}

impl AuthCheck {
    fn is_ready(&self) -> bool {
        self.status == "ready"
    }

    /// Why the provider cannot be used, worded for the user, with
    /// what they do about it. `model` is the model that was asked
    /// about.
    fn problem(&self, model: &str) -> (String, String) {
        let provider = self.provider.as_deref().unwrap_or("its provider");
        let reason = self.reason.as_deref().unwrap_or(&self.status);
        (
            format!("Pi cannot use {model}: {provider} is not ready ({reason})"),
            match &self.provider {
                Some(provider) => format!("Run `pi auth login {provider}`"),
                None => "Run `pi auth login <provider>`".to_string(),
            },
        )
    }
}

/// Ask `pi` whether it has working credentials for the provider of
/// `model`. Pi signs in to each model provider on its own, so the
/// question is always about one model. An error is a `pi` that could
/// not be run or whose answer could not be read.
pub(crate) fn auth_check(pi: &Path, model: &str) -> Result<AuthCheck, String> {
    let output = Command::new(pi)
        .args(["auth", "check", "--json", "--model", model])
        .output()
        .map_err(|e| format!("running `pi auth check`: {e}"))?;
    // Pi exits non-zero for credentials that are not ready and still
    // prints its answer
    parse_auth_check(&output.stdout).map_err(|e| {
        let said = String::from_utf8_lossy(&output.stderr);
        format!("reading `pi auth check` output: {e} {}", said.trim())
            .trim()
            .to_string()
    })
}

fn parse_auth_check(stdout: &[u8]) -> Result<AuthCheck, String> {
    serde_json::from_slice(stdout).map_err(|e| e.to_string())
}

/// Whether an agent can start on `model`: `pi` is on PATH and has
/// working credentials for the model's provider. The error is the
/// reason, worded for the user.
pub(crate) fn check_auth(model: &str) -> Result<(), String> {
    let pi = require_pi()?;
    let check = auth_check(&pi, model)?;
    if check.is_ready() {
        return Ok(());
    }
    let (message, remedy) = check.problem(model);
    Err(format!("{message}. {remedy}."))
}

/// The sign-in state of the CLI at `pi` for the provider of `model`.
pub(crate) fn auth_status(pi: &Path, model: &str) -> Field<AuthInfo> {
    match auth_check(pi, model) {
        Ok(check) if check.is_ready() => Field::Ok(AuthInfo {
            logged_in: true,
            provider: check.provider,
            method: check.auth_type,
            ..AuthInfo::default()
        }),
        Ok(check) => {
            let (message, remedy) = check.problem(model);
            Field::Problem {
                info: None,
                message,
                remedy,
            }
        }
        Err(e) => Field::Problem {
            info: None,
            message: format!("Pi sign-in state unavailable: {e}"),
            remedy: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_what_the_version_line_says() {
        assert_eq!(version_of("1.5.2\n").as_deref(), Some("1.5.2"));
        assert_eq!(version_of(""), None);
    }

    #[test]
    fn ready_credentials_name_their_provider_and_method() {
        let check =
            parse_auth_check(br#"{"status":"ready","provider":"anthropic","authType":"oauth"}"#)
                .unwrap();
        assert!(check.is_ready());
        assert_eq!(check.provider.as_deref(), Some("anthropic"));
        assert_eq!(check.auth_type.as_deref(), Some("oauth"));
    }

    #[test]
    fn credentials_that_are_not_ready_say_why_and_what_to_run() {
        let check = parse_auth_check(
            br#"{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}"#,
        )
        .unwrap();
        assert!(!check.is_ready());
        let (message, remedy) = check.problem("openai/gpt-6.1-terra");
        assert_eq!(
            message,
            "Pi cannot use openai/gpt-6.1-terra: openai is not ready \
             (credentials_not_configured)"
        );
        assert_eq!(remedy, "Run `pi auth login openai`");
        assert!(parse_auth_check(b"not json").is_err());
    }
}
