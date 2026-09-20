//! DataFusion `TableProvider`s for Claude sessions, plus their
//! supporting cache and walker.
//!
//! Every provider here is registered on a gage-query `SessionContext`
//! by [`crate::driver::ClaudeDriver::tables`]. The providers implement
//! DataFusion's pushdown surface (filter, sort, limit, projection) so
//! a query like `SELECT ... FROM session ORDER BY mtime DESC LIMIT 20`
//! opens only 20 metadata reads out of the whole corpus.

pub mod cache;
pub mod entry;
pub mod filter;
pub mod message;
pub mod session;
pub mod walk;

pub use cache::SessionCache;
pub use entry::EntryTable;
pub use message::MessageTable;
pub use session::SessionTable;
pub use walk::reconcile_for_query;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::index::IndexStore;

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
pub enum SessionSource {
    Corpus(Arc<IndexStore>),
    Lookup(Arc<HashMap<String, PathBuf>>),
}
