//! Git backed Gage store.
//!
//! Every store operation runs the `git` binary found on `PATH`. No Git
//! library is linked: the store must interoperate with other Git
//! repositories (clone, push, pull), and the binary is the only complete
//! implementation of that surface.
//!
//! Module map:
//!
//! - [`admin`] --- store administration (`init`, `status`, `fsck`,
//!   `gc`, `store_path`).
//! - [`error`] --- the crate's error type.
//! - [`git`] --- generic shell-outs to `git`: launching commands,
//!   `ls-tree`, `cat-file`, commit-object parsing. No Gage concepts.
//! - [`writer`] --- blob, tree, and commit writing under the Gage
//!   identity.
//! - [`object`] --- the object model: ref layout, markers, `parent`,
//!   link files, and the one create, edit, and delete path every type
//!   uses. Payload agnostic.
//! - `note`, `dataset`, `session` --- object types: each supplies its
//!   `attrs.json` shape, its content files, and its decoder.

mod admin;
mod dataset;
mod error;
pub mod git;
mod note;
pub mod object;
mod session;
mod writer;

pub use admin::{
    GcOutcome, InitOutcome, Remote, STORE_VERSION, StoreStatus, fsck, fsck_at, gc, gc_at, init,
    init_at, status, status_at, store_path,
};
pub use dataset::{
    DatasetRecord, DatasetSessionAddOutcome, DatasetSessionSummary, SessionMeta, SessionSpec,
    dataset_list, dataset_list_at, dataset_new, dataset_new_at, dataset_resolve_id,
    dataset_resolve_id_at, dataset_session_content, dataset_session_content_at,
    dataset_session_meta, dataset_session_meta_at, dataset_sessions_add, dataset_sessions_add_at,
    dataset_sessions_list, dataset_sessions_list_at,
};
pub use error::StoreError;
pub use git::{EntryKind, TreeEntry, cat, cat_at, ls, ls_at};
pub use note::{
    NoteFull, NoteInput, NoteRecord, note_delete, note_delete_at, note_edit, note_edit_at,
    note_get, note_get_at, note_list, note_list_at, note_new, note_new_at,
};
pub use session::{
    SessionAddOutcome, SessionAttrs, SessionOutcome, SessionRecord, session_add_at,
    session_at_commit, session_object_id,
};

pub(crate) use admin::exists;
