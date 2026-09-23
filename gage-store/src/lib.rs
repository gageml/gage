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
//! - [`object`] --- the object model: ref layout, markers, `parent`,
//!   link files, and the one create, edit, and delete path every type
//!   uses. Payload agnostic.
//! - [`index`] --- the object index: the `ObjectIndex` trait, the
//!   reconcile that keeps it current, and the query core. Type modules
//!   opt attributes in through `INDEXED_ATTRS`.
//! - [`sqlite_index`] --- the SQLite implementation of the index.
//! - `note`, `dataset`, `session` --- object types: each supplies its
//!   `attrs.json` shape, its content files, its decoder, and its typed
//!   store.

mod admin;
mod content;
mod dataset;
mod error;
pub mod git;
pub mod index;
mod note;
pub mod object;
pub mod query;
mod session;
mod sqlite_index;
mod store;
#[cfg(test)]
pub(crate) mod test_support;
pub mod url;
mod writer;

pub use admin::{GcOutcome, InitOutcome, Remote, STORE_VERSION, StoreStatus, init, store_path};
pub use dataset::{
    DatasetMembers, DatasetQuery, DatasetRecord, DatasetSessionAddOutcome, DatasetSessionSummary,
    DatasetSessionUnlinkOutcome, DatasetStore, OBJECT_TYPE as DATASET_TYPE, SessionSpec,
};
pub use error::StoreError;
pub use git::{CommitMeta, EntryKind, TreeEntry};
pub use index::{IdMatch, Order, SelectedTip};
pub use note::{
    NoteEdit, NoteFull, NoteInput, NoteQuery, NoteRecord, NoteStore, NoteValue,
    OBJECT_TYPE as NOTE_TYPE,
};
pub use object::SHORT_PREFIX_SET_SIZE;
pub use query::StoredSessionTable;
pub use session::{
    OBJECT_TYPE as SESSION_TYPE, SessionAddOutcome, SessionAttrsRecord, SessionOutcome,
    SessionQuery, SessionRecord, SessionRemoveOutcome, SessionStore, SummaryAttrs,
    session_object_id,
};
pub use sqlite_index::INDEX_SCHEMA_VERSION;
pub use store::{INDEX_FILE, Store};
