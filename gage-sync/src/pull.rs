use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use gage_core::config::{Remote, gage_home};

use crate::backend::{SyncError, open_backend};
use crate::observer::Observer;

/// Source of a pull: a configured remote or an ad-hoc local directory.
#[derive(Debug, Clone)]
pub enum PullSource {
    Remote(Remote),
    Local(PathBuf),
}

/// Default target directory when `--target` is not provided.
///
/// Sibling of `gage_home()` so a custom `GAGE_HOME` also relocates the
/// pull target.
pub fn default_pull_dir() -> PathBuf {
    let gh = gage_home();
    let parent = gh.parent().unwrap_or_else(|| Path::new("/"));
    let name = match gh.file_name().and_then(|s| s.to_str()) {
        Some(".gage") => ".gage-pull".to_string(),
        Some(other) => format!("{other}-pull"),
        None => ".gage-pull".to_string(),
    };
    parent.join(name)
}

/// Pulls from `source` into `target`. Honors `cancel` for cooperative
/// interrupt.
pub async fn pull(
    source: PullSource,
    target: Option<&Path>,
    observer: Arc<dyn Observer>,
    cancel: CancellationToken,
) -> Result<(), SyncError> {
    let remote = match source {
        PullSource::Remote(r) => r,
        PullSource::Local(path) => Remote {
            name: format!("{}", path.display()),
            kind: gage_core::config::RemoteKind::Local { path },
        },
    };

    let owned;
    let target = match target {
        Some(p) => p,
        None => {
            owned = default_pull_dir();
            &owned
        }
    };

    std::fs::create_dir_all(target)?;
    observer.remote_start(&remote.name);
    let backend = open_backend(&remote)?;
    match backend
        .fetch_all(target, Arc::clone(&observer), cancel)
        .await
    {
        Ok(()) => {
            observer.remote_finish(&remote.name, Ok(()));
            Ok(())
        }
        Err(SyncError::Interrupted) => {
            observer.remote_finish(&remote.name, Err("interrupted"));
            Err(SyncError::Interrupted)
        }
        Err(e) => {
            let msg = e.to_string();
            observer.remote_finish(&remote.name, Err(&msg));
            Err(e)
        }
    }
}
