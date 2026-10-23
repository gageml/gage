//! Derived data layer for Claude session files.
//!
//! Two artifacts shadow the session corpus, maintained by one
//! reconcile pass: a per-session summary cache at
//! `<cache root>/summary/<slug>` and a full-text index over message
//! text at `<cache root>/text/<slug>`, where the slug is the canonical
//! projects path with every non-alphanumeric character replaced by
//! `-`. Every source is keyed the same way; the default Claude
//! location is not special. Both are ephemeral: deleting either is a
//! complete reset of it.
//!
//! Layering: [`crate::session`] and [`crate::entry`] (source reading)
//! feed this module (derived artifacts); the DataFusion table
//! providers in [`crate::tables`] read through it.

mod derive;
mod paths;
mod reconcile;
mod summary_cache;
mod tool_use;

use std::fmt;

pub use derive::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_MESSAGE_SUBTYPE, COL_RAW, COL_SESSION_ID,
    COL_SUBTYPE, COL_TEXT, COL_TIMESTAMP, COL_TYPE, COL_UUID, ClaudeSessionAttrs, DerivedSession,
    Fingerprint, derive_entry, derive_session, derive_session_from, derived_schema, entry_text,
    is_message_row,
};
pub use gage_index::LockMode;
pub use reconcile::{IndexStore, ReconcileEvent, ReconcileOutcome, Status};
pub use tool_use::ToolUseCollector;

#[derive(Debug)]
pub enum IndexError {
    Io(std::io::Error),
    Arrow(arrow::error::ArrowError),
    Json(serde_json::Error),
    Text(gage_index::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Io(e) => write!(f, "io error: {e}"),
            IndexError::Arrow(e) => write!(f, "arrow error: {e}"),
            IndexError::Json(e) => write!(f, "json error: {e}"),
            IndexError::Text(e) => write!(f, "{e}"),
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

impl From<serde_json::Error> for IndexError {
    fn from(e: serde_json::Error) -> Self {
        IndexError::Json(e)
    }
}

impl From<gage_index::Error> for IndexError {
    fn from(e: gage_index::Error) -> Self {
        IndexError::Text(e)
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;
