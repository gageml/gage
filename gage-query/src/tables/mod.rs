pub mod config;
pub mod entry;
pub mod message;
pub mod message_text;
pub mod session;
pub(crate) mod walk;

pub use config::ConfigTable;
pub use entry::EntryTable;
pub use gage_index::entry_text;
pub use message::MessageTable;
pub use message_text::MessageTextFn;

use datafusion::arrow::datatypes::SchemaRef;

/// Static descriptor for a table-valued function registered on the
/// gage session context — surfaced in the repl by `\df`.
pub struct TvfInfo {
    pub name: &'static str,
    pub args: &'static str,
    pub schema: SchemaRef,
}

/// All TVFs registered by `create_context`. The repl reads this to
/// implement `\df` since DataFusion does not expose argument
/// signatures or output schemas through the `TableFunction` trait.
pub fn registered_tvfs() -> Vec<TvfInfo> {
    vec![TvfInfo {
        name: "message_text",
        args: message_text::MESSAGE_TEXT_ARGS,
        schema: message_text::message_text_schema(),
    }]
}
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
