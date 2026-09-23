//! DataFusion table providers bound to the Gage store: one per
//! object type the query surface exposes. The store owns these because
//! it knows the layout and plans every read; gage-query2 names and
//! registers them.

mod note;
mod session;

pub use note::StoredNoteTable;
pub use session::StoredSessionTable;
