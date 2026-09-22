//! [`StoreError`] and its formatters.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use crate::index::IdMatch;

#[derive(Debug)]
pub enum StoreError {
    /// No repository at the path
    NotFound(PathBuf),
    /// The repository carries no `gage.version`
    VersionMissing(PathBuf),
    /// The store's `gage.version` is not the one this build supports
    VersionMismatch { found: u32, supported: u32 },
    /// The repository uses an object format the writer does not produce
    UnsupportedObjectFormat(String),
    /// The `git` binary could not be started
    Spawn(io::Error),
    /// A file could not be written into the store
    Write { path: PathBuf, source: io::Error },
    /// Content supplied by the caller could not be read
    ReadContent(io::Error),
    /// `git` ran and exited with a failure status
    Git { status: ExitStatus, stderr: String },
    /// `git` output did not have the expected shape
    Parse(String),
    /// An object name did not resolve in the repository
    MissingObject(String),
    /// A Gage URL has no scheme or a malformed one
    BadUrl(String),
    /// A line selection fragment does not follow the grammar
    BadLineSelection(String),
    /// A target URL is well-formed but not allowed here, e.g. a
    /// fragment on a scheme that defines none
    BadTarget(String),
    /// A target URL names an object that does not exist
    TargetNotFound(String),
    /// No object matched the given id or prefix
    ObjectNotFound(String),
    /// More than one object matched the given prefix; the candidates
    /// are what the prefix could name
    AmbiguousId(String, Vec<IdMatch>),
    /// Operation refused because the object's current commit is a tombstone
    ObjectDeleted(String),
    /// Resurrection refused because the object's current commit is live
    ObjectLive(String),
    /// A tree entry name or session file path failed validation
    InvalidPath { path: String, reason: String },
    /// The object exists but is not of the type the operation requires
    WrongType {
        id: String,
        expected: String,
        actual: String,
    },
    /// No session in the dataset matched the given id
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
            StoreError::UnsupportedObjectFormat(format) => {
                write!(
                    f,
                    "store object format {format} is not supported; only sha1 is"
                )
            }
            StoreError::Spawn(e) => write!(f, "failed to run git: {e}"),
            StoreError::Write { path, source } => {
                write!(f, "failed to write {}: {source}", path.display())
            }
            StoreError::ReadContent(e) => write!(f, "failed to read content: {e}"),
            StoreError::Git { status, stderr } => write!(f, "git {status}: {stderr}"),
            StoreError::Parse(what) => write!(f, "unexpected git output: {what}"),
            StoreError::MissingObject(name) => write!(f, "no such object: {name}"),
            StoreError::BadUrl(u) => write!(f, "not a Gage URL (scheme:body): {u}"),
            StoreError::BadLineSelection(sel) => {
                write!(f, "invalid line selection: {sel}")
            }
            StoreError::BadTarget(t) => write!(f, "invalid target: {t}"),
            StoreError::TargetNotFound(t) => write!(f, "target not found: {t}"),
            StoreError::ObjectNotFound(id) => write!(f, "object not found: {id}"),
            StoreError::AmbiguousId(prefix, candidates) => {
                write!(
                    f,
                    "ambiguous id prefix {prefix}: matches {} objects:",
                    candidates.len()
                )?;
                for c in candidates {
                    let type_name = c
                        .object_type
                        .strip_prefix("gage::")
                        .unwrap_or(&c.object_type);
                    let state = if c.deleted { " (deleted)" } else { "" };
                    write!(f, "\n  {type_name} {}{state}", c.id)?;
                }
                Ok(())
            }
            StoreError::ObjectDeleted(id) => write!(f, "object is deleted: {id}"),
            StoreError::ObjectLive(id) => write!(f, "object is not deleted: {id}"),
            StoreError::InvalidPath { path, reason } => {
                write!(f, "invalid path {path:?}: {reason}")
            }
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
            StoreError::Write { source, .. } => Some(source),
            StoreError::ReadContent(e) => Some(e),
            StoreError::NotFound(_)
            | StoreError::VersionMissing(_)
            | StoreError::VersionMismatch { .. }
            | StoreError::UnsupportedObjectFormat(_)
            | StoreError::Git { .. }
            | StoreError::Parse(_)
            | StoreError::MissingObject(_)
            | StoreError::BadUrl(_)
            | StoreError::BadLineSelection(_)
            | StoreError::BadTarget(_)
            | StoreError::TargetNotFound(_)
            | StoreError::ObjectNotFound(_)
            | StoreError::AmbiguousId(..)
            | StoreError::ObjectDeleted(_)
            | StoreError::ObjectLive(_)
            | StoreError::InvalidPath { .. }
            | StoreError::WrongType { .. }
            | StoreError::SessionNotFound(_)
            | StoreError::Index(_) => None,
        }
    }
}
