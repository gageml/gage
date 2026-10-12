//! What a driver reports about itself for `gage status`: the harness
//! it integrates with, who the harness is signed in as, and Gage's
//! installation inside it. Every field is present on every driver; a
//! driver with no such concept reports [`Field::Unsupported`].

use std::path::PathBuf;

/// One status field. A `Problem` carries what the driver could still
/// read, a user-facing message, and what the user does about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    Ok(T),
    Problem {
        info: Option<T>,
        message: String,
        remedy: String,
    },
    Unsupported,
}

impl<T> Field<T> {
    /// The message and remedy of a `Problem`; `None` otherwise.
    pub fn problem(&self) -> Option<(&str, &str)> {
        match self {
            Field::Problem {
                message, remedy, ..
            } => Some((message, remedy)),
            Field::Ok(_) | Field::Unsupported => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverStatus {
    /// The harness: the application this driver integrates with,
    /// which runs agents and writes the sessions the driver reads
    pub harness: Field<HarnessInfo>,
    /// Who the harness is signed in as
    pub auth: Field<AuthInfo>,
    /// Gage's installation inside the harness, as `system_init` left it
    pub install: Field<InstallInfo>,
}

impl Default for DriverStatus {
    fn default() -> Self {
        Self {
            harness: Field::Unsupported,
            auth: Field::Unsupported,
            install: Field::Unsupported,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessInfo {
    /// Display name, e.g. "Claude Code"
    pub name: String,
    pub version: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthInfo {
    pub logged_in: bool,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub org: Option<String>,
    pub provider: Option<String>,
    pub method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInfo {
    pub installed: bool,
    pub version: Option<String>,
    /// The version this build of Gage installs
    pub expected_version: String,
    pub enabled: bool,
    pub path: Option<PathBuf>,
}
