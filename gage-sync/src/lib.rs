//! Backup transport for Gage: `push` mirrors local state to every
//! configured remote; `pull` copies a single remote's tree into an
//! inspection directory without ever touching live state.

mod backend;
mod local;
mod observer;
mod payload;
mod pull;
mod push;
mod s3;
mod ssh;

pub use backend::{Backend, SyncError};
pub use observer::{NullObserver, Observer};
pub use payload::TransferItem;
pub use pull::{PullSource, default_pull_dir, pull};
pub use push::{PushTargets, push};
