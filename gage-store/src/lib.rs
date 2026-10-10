//! Git backed Gage store.
//!
//! No Git library is linked: the store must interoperate with other Git
//! repositories (clone, push, pull), and the binary is the only complete
//! implementation of that surface. Most operations run the `git` binary
//! found on `PATH`. Two paths are not shell-outs: [`writer`] writes
//! loose objects (blobs, trees, and commits) directly into `objects/`,
//! and [`git::CatFile`] holds one long-lived `git cat-file
//! --batch-command` child per store so reads cost a pipe round trip
//! rather than a process launch. Ref updates and store administration
//! stay with the `git` binary.
//!
//! A program opens the store once with [`Store::open`] and reaches each
//! object type through a typed view over the handle: `NoteStore::from(&store)`,
//! `DatasetStore::from(&store)`, `SessionStore::from(&store)`.
//!
//! Module map:
//!
//! - [`store`] --- the [`Store`] handle: open, path, version.
//! - [`admin`] --- store administration: `init`, `store_path`, and the
//!   `status`, `fsck`, and `gc` methods.
//! - [`error`] --- the crate's error type.
//! - [`git`] --- generic shell-outs to `git`: launching commands,
//!   `ls-tree`, `cat-file`, commit-object parsing. No Gage concepts.
//! - [`writer`] --- blob, tree, and commit writing under the Gage
//!   identity.
//! - [`object`] --- the object model: ref layout, markers, `first-parent`,
//!   link files, and the one create, edit, and delete path every type
//!   uses. Payload agnostic.
//! - [`index`] --- the object index: the `ObjectIndex` trait, the
//!   reconcile that keeps it current, and the query core. Type modules
//!   opt attributes in through `INDEXED_ATTRS`.
//! - [`sqlite_index`] --- the SQLite implementation of the index.
//! - `note`, `issue`, `dataset`, `session`, `attachment`, `scan` --- object types: each supplies its
//!   `attrs.json` shape, its content files, its decoder, and its typed
//!   store.
//! - [`refs`] --- the ref layout: every ref lives under
//!   `refs/gage/<generation>/`.
//! - [`tag`] --- tags: refs under the tag namespace naming objects, and
//!   object-ish resolution (tag name, id, or unique prefix).

mod admin;
mod attachment;
mod content;
mod dataset;
mod error;
pub mod git;
pub mod index;
mod issue;
pub mod key;
mod note;
pub mod object;
pub mod query;
pub mod refs;
mod scan;
pub mod scan_dir;
mod session;
mod sqlite_index;
mod store;
mod tag;
#[cfg(test)]
pub(crate) mod test_support;
pub mod url;
mod writer;

pub use admin::{GcOutcome, InitOutcome, Remote, STORE_VERSION, StoreStatus, init, store_path};
pub use attachment::{
    AttachmentAddOutcome, AttachmentAttrs, AttachmentFile, AttachmentOutcome, AttachmentQuery,
    AttachmentRecord, AttachmentRemoveOutcome, AttachmentSpec, AttachmentStore,
    MAX_FILES as ATTACHMENT_MAX_FILES, MAX_SIZE as ATTACHMENT_MAX_SIZE,
    OBJECT_TYPE as ATTACHMENT_TYPE, selection_key_part,
};
pub use dataset::{
    AddStep, AttachmentLinkOutcome, DatasetAttachmentLinkOutcome, DatasetAttachmentUnlinkOutcome,
    DatasetAttachments, DatasetDeleted, DatasetMembers, DatasetQuery, DatasetRecord,
    DatasetSessionAddOutcome, DatasetSessionSummary, DatasetSessionUnlinkOutcome, DatasetStore,
    OBJECT_TYPE as DATASET_TYPE, SessionSpec,
};
pub use error::StoreError;
pub use git::{CommitMeta, EntryKind, TreeEntry};
pub use index::{IdMatch, IndexCounts, IndexStatus, Order, SelectedTip};
pub use issue::{
    ChangeEvent, IssueChange, IssueDirRecord, IssueFull, IssueInput, IssueQuery, IssueStatus,
    IssueStore, OBJECT_TYPE as ISSUE_TYPE, StatusReason,
};
pub use key::{KeyRef, KeyStore};
pub use note::{
    NoteEdit, NoteFull, NoteInput, NoteQuery, NoteRecord, NoteStore, NoteValue,
    OBJECT_TYPE as NOTE_TYPE,
};
pub use object::SHORT_PREFIX_SET_SIZE;
pub use query::{
    LinkKind, NoteDocRow, ScanSource, ScopedIssue, ScopedNote, StoredNoteTable, StoredSessionTable,
    attachment_file_table, attachment_table, dataset_table, issue_event_table, issue_table,
    link_table, note_doc_rows, note_doc_schema, object_key_table, scan_scope_tables, scan_table,
    scan_task_agent_table, scan_task_table, scan_watermark_table, tag_table,
};
pub use refs::{GENERATION, KEY_REFS, OBJECT_REFS, ROOT as REFS_ROOT, TAG_REFS};
pub use scan::{
    AgentAttrs, DirFiles, LOG_NAMES, OBJECT_TYPE as SCAN_TYPE, ScanAttrs, ScanContent, ScanDeleted,
    ScanFiles, ScanQuery, ScanRecord, ScanStore, ScanTask, SkipReason, TaskAgent, TaskAttrs,
    TaskCounts, TaskStatus, Watermark, agent_file_path, read_tasks,
};
pub use scan_dir::ScanDirLayout;
pub use session::{
    OBJECT_TYPE as SESSION_TYPE, SessionAddOutcome, SessionAttrsRecord, SessionOutcome,
    SessionQuery, SessionRecord, SessionRemoveOutcome, SessionStore, SummaryAttrs,
    session_object_id,
};
pub use sqlite_index::INDEX_SCHEMA_VERSION;
pub use store::{INDEX_FILE, Store};
pub use tag::{TagAdded, TagRecord, TagRef, TagStore, TagTarget};
