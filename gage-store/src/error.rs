//! [`StoreError`] and its formatters.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

#[derive(Debug)]
pub enum StoreError {
    /// No repository at the path
    NotFound(PathBuf),
    /// The repository carries no `gage.version`
    VersionMissing(PathBuf),
    /// The store's `gage.version` is not the one this build supports
    VersionMismatch { found: u32, supported: u32 },
    /// The `git` binary could not be started
    Spawn(io::Error),
    /// `git` ran and exited with a failure status
    Git { status: ExitStatus, stderr: String },
    /// `git` output did not have the expected shape
    Parse(String),
    /// A `--target` value did not match the `note:<id>` form
    BadTarget(String),
    /// A `--target` referenced an object that does not exist
    TargetNotFound(String),
    /// No object matched the given id or prefix
    ObjectNotFound(String),
    /// More than one object matched the given prefix
    AmbiguousId(String, usize),
    /// Operation refused because the object's current commit is a tombstone
    ObjectDeleted(String),
    /// The object exists but is not of the type the operation requires
    WrongType {
        id: String,
        expected: String,
        actual: String,
    },
    /// No session in the dataset matched the given num or session_id
    SessionNotFound(String),
    /// The object index could not be opened, written, or queried
    Index(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(path) => {
                write!(
                    f,
                    "no Gage store at {} (run `gage store init`)",
                    path.display()
                )
            }
            StoreError::VersionMissing(path) => {
                write!(f, "store at {} has no gage.version", path.display())
            }
            StoreError::VersionMismatch { found, supported } if found > supported => {
                write!(
                    f,
                    "store version {found} is newer than the supported version {supported}; upgrade gage"
                )
            }
            StoreError::VersionMismatch { found, supported } => {
                write!(
                    f,
                    "store version {found} is older than the supported version {supported}; no migration is available"
                )
            }
            StoreError::Spawn(e) => write!(f, "failed to run git: {e}"),
            StoreError::Git { status, stderr } => write!(f, "git {status}: {stderr}"),
            StoreError::Parse(what) => write!(f, "unexpected git output: {what}"),
            StoreError::BadTarget(t) => {
                write!(f, "invalid target {t:?}: expected `note:<id>`")
            }
            StoreError::TargetNotFound(t) => write!(f, "target not found: {t}"),
            StoreError::ObjectNotFound(id) => write!(f, "object not found: {id}"),
            StoreError::AmbiguousId(id, n) => {
                write!(f, "id {id} is ambiguous ({n} matches)")
            }
            StoreError::ObjectDeleted(id) => write!(f, "object is deleted: {id}"),
            StoreError::WrongType {
                id,
                expected,
                actual,
            } => write!(f, "{id} is a {actual}, not a {expected}"),
            StoreError::SessionNotFound(s) => write!(f, "session not found: {s}"),
            StoreError::Index(what) => write!(f, "object index: {what}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Spawn(e) => Some(e),
            StoreError::NotFound(_)
            | StoreError::VersionMissing(_)
            | StoreError::VersionMismatch { .. }
            | StoreError::Git { .. }
            | StoreError::Parse(_)
            | StoreError::BadTarget(_)
            | StoreError::TargetNotFound(_)
            | StoreError::ObjectNotFound(_)
            | StoreError::AmbiguousId(_, _)
            | StoreError::ObjectDeleted(_)
            | StoreError::WrongType { .. }
            | StoreError::SessionNotFound(_)
            | StoreError::Index(_) => None,
        }
    }
}
