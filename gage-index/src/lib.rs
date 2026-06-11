//! Derived data layer for session-file queries.
//!
//! Two artifacts shadow the session corpus, maintained by one
//! reconcile pass: a columnar store (one Parquet file of derived rows
//! per session, plus consolidated session aggregates) and a Tantivy
//! full-text index over message text. Both live under a cache
//! directory and are ephemeral: deleting the cache is a complete
//! reset.
//!
//! Layering: `gage-claude` (source reading) → `gage-index` (derived
//! artifacts) → `gage-query` (SQL surface). This crate owns
//! derivation, Parquet writing, the Tantivy index and its tokenizer
//! chain, and the reconcile pass end to end.

mod derive;
mod reconcile;
mod store;
mod text_index;

use std::fmt;

pub use derive::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_RAW, COL_SESSION_ID, COL_SUBTYPE, COL_TEXT,
    COL_TIMESTAMP, COL_TYPE, COL_UUID, DerivedSession, Fingerprint, SessionAggregates,
    derive_session, entry_text, store_schema,
};
pub use reconcile::{IndexStore, LockMode, ReconcileOutcome, Status};
pub use store::STORE_FORMAT_VERSION;
pub use text_index::{INDEX_FORMAT_VERSION, TOKENIZER_CHAIN, text_search_mask};

#[derive(Debug)]
pub enum IndexError {
    Io(std::io::Error),
    Arrow(arrow::error::ArrowError),
    Parquet(parquet::errors::ParquetError),
    Tantivy(tantivy::TantivyError),
    QueryParse(String),
    Json(serde_json::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Io(e) => write!(f, "io error: {e}"),
            IndexError::Arrow(e) => write!(f, "arrow error: {e}"),
            IndexError::Parquet(e) => write!(f, "parquet error: {e}"),
            IndexError::Tantivy(e) => write!(f, "index error: {e}"),
            IndexError::QueryParse(e) => write!(f, "invalid text_search query: {e}"),
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

impl From<parquet::errors::ParquetError> for IndexError {
    fn from(e: parquet::errors::ParquetError) -> Self {
        IndexError::Parquet(e)
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
