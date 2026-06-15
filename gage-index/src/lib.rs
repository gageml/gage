//! Derived data layer for session-file queries.
//!
//! One artifact shadows the session corpus, maintained by one
//! reconcile pass: a Tantivy full-text index over message text. The
//! index lives under a cache directory and is ephemeral: deleting the
//! cache is a complete reset.
//!
//! Layering: `gage-claude` (source reading) → `gage-index` (derived
//! artifacts) → `gage-query` (SQL surface). This crate owns
//! derivation, the Tantivy index, its tokenizer chain, and the
//! reconcile pass end to end.

mod derive;
mod reconcile;
mod text_index;

use std::fmt;

pub use derive::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_RAW, COL_SESSION_ID, COL_SUBTYPE, COL_TEXT,
    COL_TIMESTAMP, COL_TYPE, COL_UUID, DerivedSession, Fingerprint, SessionSummary, derive_session,
    derived_schema, entry_text,
};
pub use reconcile::{IndexStore, LockMode, ReconcileEvent, ReconcileOutcome, Status};
pub use text_index::{DEFAULT_SNIPPET_CHARS, Hit, INDEX_FORMAT_VERSION, TOKENIZER_CHAIN};

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
