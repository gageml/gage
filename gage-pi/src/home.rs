//! Where Pi keeps its sessions.

use std::env;
use std::path::{Path, PathBuf};

/// The directory of session files under a Pi agent directory
const SESSIONS_DIR: &str = "sessions";

/// The variable that names the agent directory in place of
/// `~/.pi/agent`
const AGENT_DIR_VAR: &str = "PI_CODING_AGENT_DIR";

/// The variable that names the sessions directory itself, wherever
/// the agent directory is
const SESSION_DIR_VAR: &str = "PI_CODING_AGENT_SESSION_DIR";

/// A source's root, which its session URLs are spelled from, and the
/// directory under it that holds the session files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub root: PathBuf,
    pub sessions_dir: PathBuf,
}

impl Location {
    /// The location the directory `dir` names: an agent directory
    /// when it holds a `sessions` directory, else a sessions directory
    /// itself. A directory that does not exist is read as a sessions
    /// directory and holds no sessions.
    pub(crate) fn with_dir(dir: &Path) -> Self {
        let sessions = dir.join(SESSIONS_DIR);
        Self {
            root: dir.to_path_buf(),
            sessions_dir: match sessions.is_dir() {
                true => sessions,
                false => dir.to_path_buf(),
            },
        }
    }

    /// The default location: the sessions directory the environment
    /// names, else the `sessions` directory of the agent directory,
    /// which is the one the environment names, else `.pi/agent` under
    /// the user's home directory. `None` when the environment names
    /// none of them. A sessions directory named on its own is its own
    /// root, so a session URL spelled from it reads back without the
    /// variable.
    pub(crate) fn default_location() -> Option<Self> {
        if let Some(dir) = var_path(SESSION_DIR_VAR) {
            return Some(Self {
                root: dir.clone(),
                sessions_dir: dir,
            });
        }
        let root = agent_dir()?;
        Some(Self {
            sessions_dir: root.join(SESSIONS_DIR),
            root,
        })
    }
}

/// The Pi agent directory, where Pi keeps its settings, its MCP
/// server registrations, and its sessions: the one the environment
/// names, else `.pi/agent` under the user's home directory. `None`
/// when the environment names neither.
pub(crate) fn agent_dir() -> Option<PathBuf> {
    var_path(AGENT_DIR_VAR)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi/agent")))
}

fn var_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_directory_is_an_agent_directory_when_it_holds_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let flat = Location::with_dir(dir.path());
        assert_eq!(flat.sessions_dir, dir.path());

        fs::create_dir(dir.path().join("sessions")).unwrap();
        let agent = Location::with_dir(dir.path());
        assert_eq!(agent.root, dir.path());
        assert_eq!(agent.sessions_dir, dir.path().join("sessions"));
    }
}
