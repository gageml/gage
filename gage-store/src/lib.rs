//! Git backed Gage store.
//!
//! Every store operation runs the `git` binary found on `PATH`. No Git
//! library is linked: the store must interoperate with other Git
//! repositories (clone, push, pull), and the binary is the only complete
//! implementation of that surface.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use gage_core::config::gage_home;

/// Path to the store: `<gage_home>/store.git`.
pub fn store_path() -> PathBuf {
    gage_home().join("store.git")
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitOutcome {
    Created,
    Reinitialized,
}

/// Creates the store at [`store_path`], or reinitializes it if present.
pub fn init() -> Result<InitOutcome, StoreError> {
    init_at(&store_path())
}

/// Creates a bare repository at `path`, or reinitializes it if present.
///
/// Leading directories are created as needed. The empty template keeps
/// sample hooks and `description` out of the store and ignores any
/// `init.templateDir` in the user's Git config. The initial branch is
/// fixed so `HEAD` does not depend on `init.defaultBranch`.
pub fn init_at(path: &Path) -> Result<InitOutcome, StoreError> {
    let existing = path.join("HEAD").is_file();
    let mut cmd = Command::new("git");
    cmd.args([
        "init",
        "--bare",
        "--quiet",
        "--template=",
        "--initial-branch=main",
    ])
    .arg(path);
    run(cmd)?;
    Ok(if existing {
        InitOutcome::Reinitialized
    } else {
        InitOutcome::Created
    })
}

fn run(mut cmd: Command) -> Result<(), StoreError> {
    let output = cmd.output().map_err(StoreError::Spawn)?;
    if output.status.success() {
        return Ok(());
    }
    Err(StoreError::Git {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

#[derive(Debug)]
pub enum StoreError {
    /// The `git` binary could not be started
    Spawn(io::Error),
    /// `git` ran and exited with a failure status
    Git { status: ExitStatus, stderr: String },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Spawn(e) => write!(f, "failed to run git: {e}"),
            StoreError::Git { status, stderr } => write!(f, "git {status}: {stderr}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Spawn(e) => Some(e),
            StoreError::Git { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_then_reinitializes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("home").join("store.git");

        assert_eq!(init_at(&store).unwrap(), InitOutcome::Created);
        assert_eq!(
            std::fs::read_to_string(store.join("HEAD")).unwrap().trim(),
            "ref: refs/heads/main"
        );
        let config = std::fs::read_to_string(store.join("config")).unwrap();
        assert!(config.contains("bare = true"), "{config}");
        assert!(!store.join("hooks").exists());

        assert_eq!(init_at(&store).unwrap(), InitOutcome::Reinitialized);
    }

    #[test]
    fn init_reports_git_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("occupied");
        std::fs::write(&file, "").unwrap();

        match init_at(&file.join("store.git")) {
            Err(StoreError::Git { stderr, .. }) => assert!(!stderr.is_empty()),
            other => panic!("expected git failure, got {other:?}"),
        }
    }
}
