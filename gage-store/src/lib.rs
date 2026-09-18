//! Git backed Gage store.
//!
//! Every store operation runs the `git` binary found on `PATH`. No Git
//! library is linked: the store must interoperate with other Git
//! repositories (clone, push, pull), and the binary is the only complete
//! implementation of that surface.
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
//! - `note`, `dataset`, `session` --- object types: each supplies its
//!   `attrs.json` shape, its content files, its decoder, and its typed
//!   store.

mod admin;
mod dataset;
mod error;
pub mod git;
mod note;
pub mod object;
mod session;
mod store;
mod writer;

pub use admin::{GcOutcome, InitOutcome, Remote, STORE_VERSION, StoreStatus, init, store_path};
pub use dataset::{
    DatasetRecord, DatasetSessionAddOutcome, DatasetSessionSummary, DatasetStore, SessionMeta,
    SessionSpec,
};
pub use error::StoreError;
pub use git::{CommitMeta, EntryKind, TreeEntry};
pub use note::{NoteFull, NoteInput, NoteRecord, NoteStore};
pub use session::{
    SessionAddOutcome, SessionAttrs, SessionOutcome, SessionRecord, SessionStore, session_object_id,
};
pub use store::Store;
