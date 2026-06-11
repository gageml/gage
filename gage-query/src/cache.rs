//! In-memory cache of derived sessions, keyed by `session_id` and
//! validated against the source file's `(mtime, size)` fingerprint.
//!
//! Lives on the DataFusion `SessionContext` as extension state. The
//! `message`, `session`, and `entry` table providers all read through
//! this cache — first touch of a session parses the JSONL into a
//! `DerivedSession`; subsequent touches stat the file and either
//! reuse the cached value or re-parse on fingerprint mismatch.
//!
//! Concurrency: a `tokio::sync::OnceCell` per slot keeps concurrent
//! first-touchers from duplicating the parse. The process is
//! short-lived (one scan run or one REPL session); there is no
//! eviction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use datafusion::error::{DataFusionError, Result};
use gage_index::{DerivedSession, Fingerprint, derive_session};
use tokio::sync::OnceCell;

type Slot = Arc<OnceCell<Arc<DerivedSession>>>;

#[derive(Default)]
pub struct SessionCache {
    map: Mutex<HashMap<String, Slot>>,
}

impl std::fmt::Debug for SessionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.map.lock().expect("cache map lock");
        f.debug_struct("SessionCache")
            .field("cached_sessions", &map.len())
            .finish()
    }
}

impl SessionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached `DerivedSession` for `session_id`, parsing
    /// `path` if absent or if the cached fingerprint no longer matches
    /// the file's stat. A fresh `stat` on every call keeps the cache
    /// honest across in-process queries.
    pub async fn get(&self, session_id: &str, path: &Path) -> Result<Arc<DerivedSession>> {
        let want = Fingerprint::stat(path)
            .map_err(|e| DataFusionError::Execution(format!("stat {}: {e}", path.display())))?;
        loop {
            let slot = self.slot(session_id);
            let id = session_id.to_string();
            let path = path.to_path_buf();
            let cached = slot
                .get_or_try_init(|| async move { parse(id, path).await })
                .await?;
            if cached.fingerprint == want {
                return Ok(Arc::clone(cached));
            }
            // Stale: drop this slot and retry. The replacement
            // `OnceCell` either parses fresh or, if another caller
            // raced ahead, reuses their parse.
            self.map.lock().expect("cache map lock").remove(session_id);
        }
    }

    fn slot(&self, session_id: &str) -> Slot {
        let mut map = self.map.lock().expect("cache map lock");
        match map.get(session_id) {
            Some(s) => Arc::clone(s),
            None => {
                let slot: Slot = Arc::new(OnceCell::new());
                map.insert(session_id.to_string(), Arc::clone(&slot));
                slot
            }
        }
    }
}

async fn parse(id: String, path: PathBuf) -> Result<Arc<DerivedSession>> {
    let derived = tokio::task::spawn_blocking(move || derive_session(&id, &path))
        .await
        .map_err(|e| DataFusionError::Execution(format!("derive task failed: {e}")))?
        .map_err(|e| DataFusionError::Execution(format!("derive failed: {e}")))?;
    Ok(Arc::new(derived))
}
