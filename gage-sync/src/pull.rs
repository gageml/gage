use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use gage_core::config::{Config, gage_home};

use crate::backend::{SyncError, open_backend};
use crate::observer::Observer;

/// Default target directory when `--into` is not provided.
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

/// Pulls a remote's tree into `target`. Refuses to write if `target`
/// exists and is non-empty. Honors `cancel` for cooperative interrupt.
pub async fn pull(
    remote_name: Option<&str>,
    target: Option<&Path>,
    observer: Arc<dyn Observer>,
    cancel: CancellationToken,
) -> Result<(), SyncError> {
    let cfg = Config::load_user().map_err(SyncError::Io)?;
    if cfg.remotes.is_empty() {
        return Err(SyncError::Config(
            "No remotes configured. Add `[[remote]]` entries to ~/.gage/config.toml".into(),
        ));
    }

    let remote = match remote_name {
        Some(name) => cfg
            .remotes
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| SyncError::Config(format!("no such remote: {name}")))?,
        None => match cfg.remotes.as_slice() {
            [only] => only,
            _ => {
                return Err(SyncError::Config(
                    "Multiple remotes configured; pass the remote name".into(),
                ));
            }
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
    let backend = open_backend(remote)?;
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
