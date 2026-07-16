use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use gage_core::config::{Remote, gage_home};
use gage_db::db::{DbError, db_path, open_db_at};
use gage_db::rusqlite::Connection;

use crate::backend::{SyncError, open_backend};
use crate::observer::Observer;
use crate::payload::build_payload;

/// Set of destinations for a push.
pub type PushTargets = Vec<Remote>;

/// Pushes the local payload to each remote in `targets`, in order.
///
/// A failure on one remote does not stop the others; this returns `Err`
/// if any remote failed. Honors `cancel` between remotes and inside
/// each backend operation.
pub async fn push(
    targets: PushTargets,
    observer: Arc<dyn Observer>,
    cancel: CancellationToken,
) -> Result<(), SyncError> {
    if targets.is_empty() {
        return Err(SyncError::Config("no push targets selected".into()));
    }

    let snapshot_dir = TempDir::new()?;
    let snapshot = snapshot_dir.path().join("gage.db");
    snapshot_db(&snapshot)?;
    let slow_snapshot = snapshot_slow_db(snapshot_dir.path())?;
    if cancel.is_cancelled() {
        return Err(SyncError::Interrupted);
    }

    let items = build_payload(&snapshot, slow_snapshot.as_deref())?;

    let mut failures: Vec<String> = Vec::new();
    let mut interrupted = false;
    for remote in &targets {
        if cancel.is_cancelled() {
            interrupted = true;
            break;
        }
        observer.remote_start(&remote.name);
        let backend = match open_backend(remote) {
            Ok(b) => b,
            Err(e) => {
                let msg = e.to_string();
                observer.remote_finish(&remote.name, Err(&msg));
                failures.push(remote.name.clone());
                continue;
            }
        };
        match backend
            .put_all(&items, Arc::clone(&observer), cancel.clone())
            .await
        {
            Ok(()) => observer.remote_finish(&remote.name, Ok(())),
            Err(SyncError::Interrupted) => {
                observer.remote_finish(&remote.name, Err("interrupted"));
                interrupted = true;
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                observer.remote_finish(&remote.name, Err(&msg));
                failures.push(remote.name.clone());
            }
        }
    }

    if interrupted {
        return Err(SyncError::Interrupted);
    }
    if !failures.is_empty() {
        return Err(SyncError::Backend(format!(
            "push failed for: {}",
            failures.join(", ")
        )));
    }
    Ok(())
}

fn snapshot_db(dest: &Path) -> Result<(), SyncError> {
    let src = db_path();
    if !src.exists() {
        return Err(SyncError::Sqlite(format!(
            "database not found at {}",
            src.display()
        )));
    }
    let conn = open_db_at(&src).map_err(|e: DbError| SyncError::Sqlite(e.to_string()))?;
    vacuum_into(&conn, dest)
}

/// Snapshots `log/slow.db` into `dir` when a slow query log exists.
///
/// Opened directly rather than through `open_db_at`, which would run the
/// gage schema migrations against it.
fn snapshot_slow_db(dir: &Path) -> Result<Option<PathBuf>, SyncError> {
    let src = gage_home().join("log").join("slow.db");
    if !src.is_file() {
        return Ok(None);
    }
    let conn = Connection::open(&src).map_err(|e| SyncError::Sqlite(e.to_string()))?;
    let dest = dir.join("slow.db");
    vacuum_into(&conn, &dest)?;
    Ok(Some(dest))
}

fn vacuum_into(conn: &Connection, dest: &Path) -> Result<(), SyncError> {
    let dest_str = dest.to_string_lossy();
    let escaped = dest_str.replace('\'', "''");
    let sql = format!("VACUUM INTO '{escaped}'");
    conn.execute_batch(&sql)
        .map_err(|e| SyncError::Sqlite(format!("VACUUM INTO failed: {e}")))?;
    Ok(())
}
