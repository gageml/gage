pub mod config;
pub mod entry;
pub mod issue;
pub mod issue_evidence;
pub mod message;
pub mod note;
pub mod session;
pub(crate) mod store_scan;

pub use config::ConfigTable;
pub use entry::EntryTable;
// Re-exported from the derivation module that owns it now.
pub use gage_index::entry_text;
pub use issue::IssueTable;
pub use issue_evidence::IssueEvidenceTable;
pub use message::MessageTable;
pub use note::NoteTable;
pub use session::SessionTable;

use std::path::PathBuf;
use std::sync::Arc;

use gage_index::IndexStore;

/// Where a session-row table provider (`EntryTable`, `MessageTable`)
/// finds its data.
///
/// `Store` scans the derived columnar store for a whole corpus,
/// reconciling first — the global `gage query` use case.
/// `SingleSession` derives rows from exactly one session file in
/// memory, which is the per-session scanner context built by
/// `gage-scan`'s runner.
#[derive(Debug, Clone)]
pub(super) enum SessionSource {
    Store(Arc<IndexStore>),
    SingleSession { session_id: String, path: PathBuf },
}
