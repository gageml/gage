//! Derived data layer for Claude session files.
//!
//! One artifact shadows the session corpus, maintained by one
//! reconcile pass: a Tantivy full-text index over message text. The
//! index lives under a cache directory and is ephemeral: deleting the
//! cache is a complete reset.
//!
//! Layering: [`crate::session`] and [`crate::entry`] (source reading)
//! feed this module (derived artifacts); the DataFusion table
//! providers in [`crate::tables`] read through it.

mod derive;
mod reconcile;
mod summary_cache;
mod text_index;

use std::fmt;
use std::path::{Path, PathBuf};

use gage_core::config::gage_home;

use crate::session::encode_project_dir;

pub use derive::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_MESSAGE_SUBTYPE, COL_RAW, COL_SESSION_ID,
    COL_SUBTYPE, COL_TEXT, COL_TIMESTAMP, COL_TYPE, COL_UUID, DerivedSession, Fingerprint,
    SessionSummary, derive_session, derived_schema, entry_text, is_message_row,
};
pub use reconcile::{
    IndexStore, LockMode, ReconcileEvent, ReconcileOutcome, SOURCE_MARKER, Status,
};
pub use text_index::{DEFAULT_SNIPPET_CHARS, Hit, INDEX_FORMAT_VERSION, TOKENIZER_CHAIN};

/// Cache directory for the index artifacts of one projects directory:
/// `<gage_home>/cache/<slug>`, where the slug is the canonical projects
/// path with every non-alphanumeric character replaced by `-`. Every
/// source is keyed the same way; the default Claude location is not
/// special.
pub fn cache_dir_for(projects_dir: &Path) -> PathBuf {
    // A projects dir that cannot be canonicalized (typically: it does
    // not exist yet) is keyed as given. The only effect of a
    // non-canonical key is a second cache dir for the same corpus.
    let canonical =
        std::fs::canonicalize(projects_dir).unwrap_or_else(|_| projects_dir.to_path_buf());
    gage_home()
        .join("cache")
        .join(encode_project_dir(&canonical))
}

#[derive(Debug)]
pub enum IndexError {
    Io(std::io::Error),
    Arrow(arrow::error::ArrowError),
    Tantivy(tantivy::TantivyError),
    QueryParse(String),
    Json(serde_json::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Io(e) => write!(f, "io error: {e}"),
            IndexError::Arrow(e) => write!(f, "arrow error: {e}"),
            IndexError::Tantivy(e) => write!(f, "index error: {e}"),
            IndexError::QueryParse(e) => write!(f, "invalid text search query: {e}"),
            IndexError::Json(e) => write!(f, "json error: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        IndexError::Io(e)
    }
}

impl From<arrow::error::ArrowError> for IndexError {
    fn from(e: arrow::error::ArrowError) -> Self {
        IndexError::Arrow(e)
    }
}

impl From<tantivy::TantivyError> for IndexError {
    fn from(e: tantivy::TantivyError) -> Self {
        IndexError::Tantivy(e)
    }
}

impl From<serde_json::Error> for IndexError {
    fn from(e: serde_json::Error) -> Self {
        IndexError::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;
