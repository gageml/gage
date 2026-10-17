//! DataFusion table providers bound to the Gage store: one per
//! object type the query surface exposes, and one per link file. The
//! store owns these because it knows the layout and plans every read;
//! gage-query names and registers them.
//!
//! Two kinds of provider. `session` and `note` plan their reads
//! against the index and open objects only for projected columns.
//! `dataset`, `attachment`, `scan`, `scan_log`, `scan_task`,
//! `scan_task_agent`, `issue`, `issue_event`, `tag`, and the link tables build one batch
//! under the store lock through [`batch::BatchTable`], which is enough
//! until an index serves them.

mod attachment;
mod batch;
mod dataset;
mod issue;
mod issue_report;
mod key;
mod links;
mod note;
mod note_doc;
mod related_issue;
mod scan;
mod scan_log;
mod scan_scope;
mod scan_task;
mod session;
mod tag;
mod watermark;

pub use attachment::{attachment_file_table, attachment_table};
pub use dataset::dataset_table;
pub use issue::{issue_event_table, issue_table};
pub use issue_report::issue_report_fn;
pub use key::object_key_table;
pub use links::{LinkKind, link_table};
pub use note::StoredNoteTable;
pub use note_doc::{NoteDocRow, note_doc_rows, note_doc_schema};
pub use related_issue::related_issue_fn;
pub use scan::scan_table;
pub use scan_log::scan_log_table;
pub use scan_scope::{ScanSource, ScopedIssue, ScopedNote, scan_scope_tables};
pub use scan_task::{scan_task_agent_table, scan_task_table};
pub use session::StoredSessionTable;
pub use tag::tag_table;
pub use watermark::scan_watermark_table;
