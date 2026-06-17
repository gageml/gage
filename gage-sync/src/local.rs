use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::backend::{Backend, SyncError};
use crate::observer::Observer;
use crate::payload::TransferItem;

/// Local-directory backend. Copies files directly to/from a directory
/// on the local filesystem. No rsync, no external process.
///
/// `root` is the absolute, tilde-expanded directory the payload lands
/// under: each [`TransferItem::remote`] resolves to `<root>/<remote>`.
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Backend for LocalBackend {
    async fn put_all(
        &self,
        items: &[TransferItem],
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError> {
        tokio::fs::create_dir_all(&self.root).await?;
        for item in items {
            if cancel.is_cancelled() {
                return Err(SyncError::Interrupted);
            }
            let dest = self.root.join(&item.remote);
            copy_path(&item.local, &dest, observer.as_ref(), &cancel).await?;
        }
        Ok(())
    }

    async fn fetch_all(
        &self,
        local: &Path,
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError> {
        if !self.root.exists() {
            return Err(SyncError::Backend(format!(
                "local source does not exist: {}",
                self.root.display()
            )));
        }
        tokio::fs::create_dir_all(local).await?;
        copy_path(&self.root, local, observer.as_ref(), &cancel).await
    }
}

async fn copy_path(
    src: &Path,
    dest: &Path,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Result<(), SyncError> {
    let metadata = tokio::fs::symlink_metadata(src).await?;
    let file_type = metadata.file_type();
    if file_type.is_file() {
        copy_file(src, dest, observer).await
    } else if file_type.is_dir() {
        copy_dir(src, dest, observer, cancel).await
    } else if file_type.is_symlink() {
        // Resolve through symlinks; payload uses real paths but a local
        // remote can have its own.
        let target = tokio::fs::canonicalize(src).await?;
        Box::pin(copy_path(&target, dest, observer, cancel)).await
    } else {
        Err(SyncError::Backend(format!(
            "unsupported file type at {}",
            src.display()
        )))
    }
}

async fn copy_file(src: &Path, dest: &Path, observer: &dyn Observer) -> Result<(), SyncError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::copy(src, dest).await?;
    observer.line(&format!("{}", dest.display()));
    Ok(())
}

async fn copy_dir(
    src: &Path,
    dest: &Path,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Result<(), SyncError> {
    let src = src.to_path_buf();
    let dest = dest.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || {
        WalkDir::new(&src)
            .follow_links(true)
            .into_iter()
            .map(|r| r.map(|e| e.into_path()))
            .collect::<Result<Vec<_>, _>>()
            .map(|paths| (src, paths))
    })
    .await
    .map_err(|e| SyncError::Backend(format!("walkdir join: {e}")))?
    .map_err(|e| SyncError::Backend(format!("walkdir: {e}")))?;
    let (src_root, paths) = entries;

    for path in paths {
        if cancel.is_cancelled() {
            return Err(SyncError::Interrupted);
        }
        let rel = path
            .strip_prefix(&src_root)
            .map_err(|e| SyncError::Backend(format!("walkdir prefix mismatch: {e}")))?;
        let target = if rel.as_os_str().is_empty() {
            dest.clone()
        } else {
            dest.join(rel)
        };
        let md = tokio::fs::symlink_metadata(&path).await?;
        if md.file_type().is_dir() {
            tokio::fs::create_dir_all(&target).await?;
        } else if md.file_type().is_file() {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&path, &target).await?;
            observer.line(&format!("{}", target.display()));
        }
    }
    Ok(())
}
