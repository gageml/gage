pub mod config;
pub mod entry;
pub mod issue;
pub mod issue_evidence;
pub mod message;
pub mod message_text;
pub mod note;
pub mod session;
pub(crate) mod walk;

pub use config::ConfigTable;
pub use entry::EntryTable;
pub use gage_index::entry_text;
pub use issue::IssueTable;
pub use issue_evidence::IssueEvidenceTable;
pub use message::MessageTable;
pub use message_text::MessageTextFn;
pub use note::NoteTable;
pub use session::SessionTable;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gage_index::IndexStore;

/// Where a session-row table provider (`EntryTable`, `MessageTable`)
/// finds its data.
///
/// `Corpus` scans the corpus for a whole project, reconciling first and
/// reading through the per-context session cache — the global `gage
/// query` use case. `Lookup` resolves session ids through an explicit
/// id-to-path map and reads through the per-context session cache —
/// used by gage-scan, whose scan run already enumerated the cohort and
/// has no need for the corpus index or reconcile.
#[derive(Debug, Clone)]
pub(super) enum SessionSource {
    Corpus(Arc<IndexStore>),
    Lookup(Arc<HashMap<String, PathBuf>>),
}
