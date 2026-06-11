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

use std::path::PathBuf;
use std::sync::Arc;

use gage_index::IndexStore;

/// Where a session-row table provider (`EntryTable`, `MessageTable`)
/// finds its data.
///
/// `Corpus` scans the corpus for a whole project, reconciling first
/// and reading through the per-context session cache — the global
/// `gage query` use case. `SingleSession` parses one session file in
/// memory and bypasses the cache — used by gage-scan's per-session
/// scanner context.
#[derive(Debug, Clone)]
pub(super) enum SessionSource {
    Corpus(Arc<IndexStore>),
    SingleSession { session_id: String, path: PathBuf },
}
