//! Backup transport for Gage: `push` mirrors local state to every
//! configured remote; `pull` copies a single remote's tree into an
//! inspection directory without ever touching live state.

mod backend;
mod observer;
mod payload;
mod pull;
mod push;
mod s3;
mod ssh;

pub use backend::{Backend, SyncError};
pub use observer::{NullObserver, Observer};
pub use payload::TransferItem;
pub use pull::pull;
pub use push::push;
