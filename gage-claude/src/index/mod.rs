//! Derivation of Claude session files, for the derived data layer
//! `gage_corpus` keeps over them: the summary cache, the full-text
//! index, and the DataFusion table providers.
//!
//! Layering: [`crate::session`] and [`crate::entry`] (source reading)
//! feed this module, which the corpus the driver hands to
//! `gage_corpus` derives through.

mod corpus;
mod derive;
mod tool_use;

pub use corpus::{ClaudeCorpus, index_store};
pub use derive::{
    ClaudeSessionAttrs, SUMMARY_VERSION, derive_entry, derive_session, derive_session_from,
    entry_text, is_message_row, project_of,
};
pub use gage_corpus::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_MESSAGE_SUBTYPE, COL_MESSAGE_TYPE, COL_RAW,
    COL_SESSION_ID, COL_SUBTYPE, COL_TEXT, COL_TIMESTAMP, COL_TYPE, COL_UUID, DerivedSession,
    Fingerprint, IndexError, IndexStore, LockMode, ReconcileOutcome, Result, Status,
    derived_schema,
};
pub use tool_use::ToolUseCollector;
