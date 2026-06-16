use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::backend::{Backend, SyncError};
use crate::observer::Observer;
use crate::payload::TransferItem;

/// S3 (and S3-compatible) backend backed by `object_store`. Credentials
/// and region come from the standard AWS chain (env vars, `~/.aws/...`,
/// IMDS), overridable from config.
pub struct S3Backend {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3Backend {
    pub fn new(url: &str, region: Option<&str>, endpoint: Option<&str>) -> Result<Self, SyncError> {
        let (bucket, prefix) = parse_s3_url(url)?;
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&bucket);
        if let Some(r) = region {
            builder = builder.with_region(r);
        }
        if let Some(e) = endpoint {
            builder = builder.with_endpoint(e);
        }
        let store = builder
            .build()
            .map_err(|e| SyncError::Backend(format!("s3 init: {e}")))?;
        Ok(Self {
            store: Arc::new(store),
            prefix,
        })
    }

    fn key(&self, rel: &str) -> ObjPath {
        let joined = if self.prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{}/{}", self.prefix, rel)
        };
        ObjPath::from(joined)
    }
}

fn parse_s3_url(url: &str) -> Result<(String, String), SyncError> {
    let rest = url
        .strip_prefix("s3://")
        .ok_or_else(|| SyncError::Config(format!("expected s3:// URL, got: {url}")))?;
    let (bucket, prefix) = match rest.split_once('/') {
        Some((b, p)) => (b.to_string(), p.trim_end_matches('/').to_string()),
        None => (rest.to_string(), String::new()),
    };
    if bucket.is_empty() {
        return Err(SyncError::Config(format!("missing bucket in: {url}")));
    }
    Ok((bucket, prefix))
}

const UPLOAD_CONCURRENCY: usize = 8;

#[async_trait]
impl Backend for S3Backend {
    async fn put_all(
        &self,
        items: &[TransferItem],
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError> {
        let mut uploads: Vec<(PathBuf, String)> = Vec::new();
        for item in items {
            let meta = std::fs::metadata(&item.local)?;
            if meta.is_file() {
                uploads.push((item.local.clone(), item.remote.clone()));
            } else if meta.is_dir() {
                for entry in WalkDir::new(&item.local).into_iter() {
                    let entry = entry.map_err(|e| SyncError::Backend(e.to_string()))?;
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let rel = entry
                        .path()
                        .strip_prefix(&item.local)
                        .expect("walkdir paths are under root");
                    let key = join_key(&item.remote, rel);
                    uploads.push((entry.path().to_path_buf(), key));
                }
            }
        }

        let work = futures::stream::iter(uploads.into_iter().map(|(local, rel)| {
            let store = Arc::clone(&self.store);
            let prefix = self.prefix.clone();
            let observer = Arc::clone(&observer);
            async move { put_one(&store, &prefix, &local, &rel, observer.as_ref()).await }
        }))
        .buffer_unordered(UPLOAD_CONCURRENCY)
        .collect::<Vec<_>>();

        let results = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(SyncError::Interrupted),
            r = work => r,
        };
        results.into_iter().collect::<Result<Vec<()>, _>>()?;
        Ok(())
    }

    async fn fetch_all(
        &self,
        local: &Path,
        observer: Arc<dyn Observer>,
        cancel: CancellationToken,
    ) -> Result<(), SyncError> {
        tokio::fs::create_dir_all(local).await?;
        let list_prefix = if self.prefix.is_empty() {
            None
        } else {
            Some(self.key(""))
        };

        let list_fut = async {
            let mut keys = Vec::new();
            let mut stream = self.store.list(list_prefix.as_ref());
            while let Some(meta) = stream.next().await {
                let meta = meta.map_err(|e| SyncError::Backend(format!("s3 list: {e}")))?;
                keys.push(meta.location);
            }
            Ok::<_, SyncError>(keys)
        };

        let keys = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(SyncError::Interrupted),
            r = list_fut => r?,
        };

        let prefix = self.prefix.clone();
        let local_owned = local.to_path_buf();
        let work = futures::stream::iter(keys.into_iter().map(|key| {
            let store = Arc::clone(&self.store);
            let prefix = prefix.clone();
            let local = local_owned.clone();
            let observer = Arc::clone(&observer);
            async move { fetch_one(&store, &prefix, &key, &local, observer.as_ref()).await }
        }))
        .buffer_unordered(UPLOAD_CONCURRENCY)
        .collect::<Vec<_>>();

        let results = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(SyncError::Interrupted),
            r = work => r,
        };
        results.into_iter().collect::<Result<Vec<()>, _>>()?;
        Ok(())
    }
}

fn join_key(remote_dir: &str, rel: &Path) -> String {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if remote_dir.is_empty() {
        rel_str
    } else {
        format!("{remote_dir}/{rel_str}")
    }
}

async fn put_one(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    local: &Path,
    rel: &str,
    observer: &dyn Observer,
) -> Result<(), SyncError> {
    let bytes = tokio::fs::read(local).await?;
    let key = if prefix.is_empty() {
        ObjPath::from(rel.to_string())
    } else {
        ObjPath::from(format!("{prefix}/{rel}"))
    };
    store
        .put(&key, bytes.into())
        .await
        .map_err(|e| SyncError::Backend(format!("s3 put {key}: {e}")))?;
    observer.line(&format!("put {key}"));
    Ok(())
}

async fn fetch_one(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    key: &ObjPath,
    local_root: &Path,
    observer: &dyn Observer,
) -> Result<(), SyncError> {
    let key_str = key.as_ref();
    let rel = if prefix.is_empty() {
        key_str
    } else {
        key_str
            .strip_prefix(prefix)
            .and_then(|s| s.strip_prefix('/'))
            .unwrap_or(key_str)
    };
    let dest = local_root.join(rel);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let get = store
        .get(key)
        .await
        .map_err(|e| SyncError::Backend(format!("s3 get {key}: {e}")))?;
    let bytes = get
        .bytes()
        .await
        .map_err(|e| SyncError::Backend(format!("s3 read {key}: {e}")))?;
    tokio::fs::write(&dest, bytes).await?;
    observer.line(&format!("get {key}"));
    Ok(())
}
