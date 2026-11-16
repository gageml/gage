//! The user's `codex` CLI: where it is, its version, and whether it
//! is signed in. Nothing here starts a session or uses tokens.

use std::path::{Path, PathBuf};
use std::process::Command;

use gage_session::{AuthInfo, Field, HarnessInfo};

/// The harness's display name
pub(crate) const HARNESS_NAME: &str = "Codex";

const NOT_FOUND: &str = "The `codex` CLI was not found on PATH. Install Codex and try again.";

const NOT_SIGNED_IN: &str = "Not signed in to Codex. Run `codex login`.";

/// The `codex` binary on PATH.
pub(crate) fn find_codex() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("codex"))
        .find(|candidate| candidate.is_file())
}

/// The `codex` binary on PATH, or why there is none, worded for the
/// user.
pub(crate) fn require_codex() -> Result<PathBuf, String> {
    find_codex().ok_or_else(|| NOT_FOUND.to_string())
}

/// What `codex login status` says. It exits 0 when signed in and
/// prints one line naming how, `Logged in using ChatGPT`, on stderr.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Login {
    /// Signed in; the method as Codex names it, when it names one
    SignedIn(Option<String>),
    SignedOut,
}

/// Ask `codex` whether it is signed in. An error is a `codex` that
/// could not be run.
pub(crate) fn login(codex: &Path) -> Result<Login, String> {
    let output = Command::new(codex)
        .args(["login", "status"])
        .output()
        .map_err(|e| format!("running `codex login status`: {e}"))?;
    if !output.status.success() {
        return Ok(Login::SignedOut);
    }
    let said = [output.stderr, output.stdout].concat();
    Ok(Login::SignedIn(login_method(&String::from_utf8_lossy(
        &said,
    ))))
}

/// The method in a `Logged in using <method>` line
fn login_method(said: &str) -> Option<String> {
    said.lines()
        .find_map(|line| line.trim().strip_prefix("Logged in using "))
        .map(|method| method.trim().to_string())
        .filter(|method| !method.is_empty())
}

/// Whether an agent can start: `codex` is on PATH and signed in. The
/// error is the reason, worded for the user.
pub(crate) fn check_auth() -> Result<(), String> {
    let codex = require_codex()?;
    match login(&codex)? {
        Login::SignedIn(_) => Ok(()),
        Login::SignedOut => Err(NOT_SIGNED_IN.to_string()),
    }
}

/// The CLI's location and version. `Absent` when it is not installed.
pub(crate) fn harness_status() -> Field<HarnessInfo> {
    let Some(path) = find_codex() else {
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

/// The version in `codex --version` output, `codex-cli 0.169.0`
fn version_of(said: &str) -> Option<String> {
    said.split_whitespace().last().map(String::from)
}

/// The sign-in state of the CLI at `codex`.
pub(crate) fn auth_status(codex: &Path) -> Field<AuthInfo> {
    match login(codex) {
        Ok(Login::SignedIn(method)) => Field::Ok(AuthInfo {
            logged_in: true,
            method,
            ..AuthInfo::default()
        }),
        Ok(Login::SignedOut) => Field::Problem {
            info: None,
            message: "Not signed in to Codex".to_string(),
            remedy: "Run `codex login`".to_string(),
        },
        Err(e) => Field::Problem {
            info: None,
            message: format!("Codex sign-in state unavailable: {e}"),
            remedy: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_method_is_read_from_the_status_line() {
        assert_eq!(
            login_method("Logged in using ChatGPT\n").as_deref(),
            Some("ChatGPT")
        );
        assert_eq!(login_method("something else\n"), None);
        assert_eq!(login_method(""), None);
    }

    #[test]
    fn the_version_is_the_last_word_of_the_version_line() {
        assert_eq!(
            version_of("codex-cli 0.169.0\n").as_deref(),
            Some("0.169.0")
        );
        assert_eq!(version_of(""), None);
    }
}
