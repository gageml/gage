use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use gage_core::config::{Remote, RemoteKind};

use crate::observer::Observer;
use crate::payload::TransferItem;
use crate::s3::S3Backend;
use crate::ssh::SshBackend;

#[derive(Debug)]
pub enum SyncError {
    Io(io::Error),
    Sqlite(String),
    Backend(String),
    Config(String),
    Interrupted,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "{e}"),
            SyncError::Sqlite(s) => write!(f, "{s}"),
            SyncError::Backend(s) => write!(f, "{s}"),
            SyncError::Config(s) => write!(f, "{s}"),
            SyncError::Interrupted => write!(f, "interrupted"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        SyncError::Io(e)
    }
}

/// Abstract transport for push and pull. A backend operates relative to
/// a root configured at construction (an ssh path, an s3 prefix).
///
/// Backends emit status lines via `observer.line(...)` as work happens
/// and watch `cancel` for cooperative interruption.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Uploads every item. Each item's `local` may be a file or a
    /// directory; the layout that lands at the remote is
    /// `<root>/<item.remote>`.
    async fn put_all(
        &self,
        items: &[TransferItem],
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError>;

    /// Downloads the entire remote tree into `local`. Local layout
    /// matches the remote layout verbatim.
    async fn fetch_all(
        &self,
        local: &Path,
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError>;
}

pub fn open_backend(remote: &Remote) -> Result<Box<dyn Backend>, SyncError> {
    match &remote.kind {
        RemoteKind::Ssh { url } => Ok(Box::new(SshBackend::new(url.clone()))),
        RemoteKind::S3 {
            url,
            region,
            endpoint,
        } => Ok(Box::new(S3Backend::new(
            url,
            region.as_deref(),
            endpoint.as_deref(),
        )?)),
    }
}
