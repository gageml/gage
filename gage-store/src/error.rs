//! [`StoreError`] and its formatters.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

#[derive(Debug)]
pub enum StoreError {
    /// No repository at the path
    NotFound(PathBuf),
    /// The `git` binary could not be started
    Spawn(io::Error),
    /// `git` ran and exited with a failure status
    Git { status: ExitStatus, stderr: String },
    /// `git` output did not have the expected shape
    Parse(String),
    /// A `--target` value did not match the `note:<id>` form
    BadTarget(String),
    /// A `--target` referenced a note ref that does not exist
    TargetNotFound(String),
    /// No note ref matched the given id or prefix
    NoteNotFound(String),
    /// More than one note ref matched the given prefix
    AmbiguousNoteId(String, usize),
    /// Operation refused because the note's current commit is a tombstone
    NoteDeleted(String),
    /// No dataset ref matched the given id or prefix
    DatasetNotFound(String),
    /// More than one dataset ref matched the given prefix
    AmbiguousDatasetId(String, usize),
    /// No session in the dataset matched the given num or session_id
    SessionNotFound(String),
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
            StoreError::Spawn(e) => write!(f, "failed to run git: {e}"),
            StoreError::Git { status, stderr } => write!(f, "git {status}: {stderr}"),
            StoreError::Parse(what) => write!(f, "unexpected git output: {what}"),
            StoreError::BadTarget(t) => {
                write!(f, "invalid target {t:?}: expected `note:<id>`")
            }
            StoreError::TargetNotFound(t) => write!(f, "target not found: {t}"),
            StoreError::NoteNotFound(id) => write!(f, "note not found: {id}"),
            StoreError::AmbiguousNoteId(id, n) => {
                write!(f, "note id {id} is ambiguous ({n} matches)")
            }
            StoreError::NoteDeleted(id) => write!(f, "note is deleted: {id}"),
            StoreError::DatasetNotFound(id) => write!(f, "dataset not found: {id}"),
            StoreError::AmbiguousDatasetId(id, n) => {
                write!(f, "dataset id {id} is ambiguous ({n} matches)")
            }
            StoreError::SessionNotFound(s) => write!(f, "session not found: {s}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Spawn(e) => Some(e),
            StoreError::NotFound(_)
            | StoreError::Git { .. }
            | StoreError::Parse(_)
            | StoreError::BadTarget(_)
            | StoreError::TargetNotFound(_)
            | StoreError::NoteNotFound(_)
            | StoreError::AmbiguousNoteId(_, _)
            | StoreError::NoteDeleted(_)
            | StoreError::DatasetNotFound(_)
            | StoreError::AmbiguousDatasetId(_, _)
            | StoreError::SessionNotFound(_) => None,
        }
    }
}
