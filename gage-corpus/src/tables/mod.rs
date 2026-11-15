//! DataFusion `TableProvider`s for the sessions of a corpus, plus
//! their supporting cache and walker.
//!
//! Every provider here is registered on a gage-query `SessionContext`
//! through the `tables` of the driver's source. The providers implement
//! DataFusion's pushdown surface (filter, sort, limit, projection) so
//! a query like `SELECT ... FROM session ORDER BY modified DESC LIMIT 20`
//! opens only 20 metadata reads out of the whole corpus.

pub mod cache;
pub mod entry;
pub mod message;
pub mod session;
pub mod tool_use;
pub mod tool_use_file;
pub mod walk;

pub use cache::SessionCache;
pub use entry::EntryTable;
pub use message::MessageTable;
pub use session::SessionTable;
pub use tool_use::ToolUseTable;
pub use tool_use_file::ToolUseFileTable;
