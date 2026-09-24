//! DataFusion table providers bound to the Gage store: one per
//! object type the query surface exposes, and one per link file. The
//! store owns these because it knows the layout and plans every read;
//! gage-query2 names and registers them.
//!
//! Two kinds of provider. `session` and `note` plan their reads
//! against the index and open objects only for projected columns.
//! `dataset`, `scan`, and the link tables build one batch under the
//! store lock through [`batch::BatchTable`], which is enough until an
//! index serves them.

mod batch;
mod dataset;
mod links;
mod note;
mod scan;
mod session;

pub use dataset::dataset_table;
pub use links::{LinkKind, link_table};
pub use note::StoredNoteTable;
pub use scan::scan_table;
pub use session::StoredSessionTable;
