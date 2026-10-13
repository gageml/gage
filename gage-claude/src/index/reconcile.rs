//! The reconcile pass and the `IndexStore` handle that owns the
//! per-session summary cache.
//!
//! Any query of the session-row tables reconciles first. The pass
//! serializes on an advisory file lock. Queries try-lock and on
//! contention skip reconciliation, reading the current cache — the
//! contender is reconciling the same corpus, so staleness is bounded
//! at one pass and one-directional: missed recent rows, never wrong
//! rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::FileExt;
use serde::{Deserialize, Serialize};

use super::Result;
use super::derive::{Fingerprint, SessionSummary, derive_session};
use super::summary_cache;

/// How to take the reconcile lock. Queries use `Try` —
/// skip-with-stale on contention. Explicit maintenance (`gage index`)
/// uses `Wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Try,
    Wait,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReconcileOutcome {
    /// Reconciliation was skipped because another process holds the
    /// reconcile lock; the cache is at most one pass stale.
    pub skipped: bool,
    /// Sessions discovered on disk.
    pub discovered: usize,
    /// Sessions re-indexed.
    pub indexed: usize,
    /// Sessions removed from the cache.
    pub removed: usize,
    /// Wall-clock duration of the reconcile pass, in milliseconds.
    pub elapsed_ms: u64,
}

/// Progress signal. `Start` fires once after the diff with the
/// unit-of-work count (sessions to reindex or remove); `Advance` fires
/// after each unit completes.
#[derive(Debug, Clone, Copy)]
pub enum ReconcileEvent {
    Start { total: u64 },
    Advance,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub cache_version: u32,
    pub discovered: usize,
    pub indexed: usize,
    pub dirty: usize,
    pub last_reconcile_ms: Option<i64>,
    pub cache_dir: PathBuf,
    pub cache_bytes: u64,
}

/// Per-session cache state, written only under the reconcile lock,
/// after the summaries are written. A crash before the manifest write
/// understates the cache; the next reconcile re-derives those
/// sessions, and the summary write is idempotent.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    last_reconcile_ms: i64,
    sessions: BTreeMap<String, Fingerprint>,
}

impl Manifest {
    fn empty() -> Self {
        Self {
            format_version: summary_cache::VERSION,
            last_reconcile_ms: 0,
            sessions: BTreeMap::new(),
        }
    }

    fn is_current(&self) -> bool {
        self.format_version == summary_cache::VERSION
    }
}

/// In-process serialization of reconcile passes, keyed by cache dir.
/// The file lock serializes across processes; this mutex keeps
/// concurrent scans within one process from duplicating work (and
/// from try-lock-failing against their own siblings).
static PROCESS_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn process_lock(cache_dir: &Path) -> Arc<Mutex<()>> {
    let mut map = PROCESS_LOCKS.lock().expect("process lock registry");
    map.entry(cache_dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Handle to the summary cache for one session corpus.
///
/// `root` is the Claude projects directory the corpus is read from;
/// `cache_dir` holds the cache (`~/.gage/cache` in production).
/// Everything under `cache_dir` is rebuildable; deleting it is a
/// complete reset.
#[derive(Debug, Clone)]
pub struct IndexStore {
    root: PathBuf,
    cache_dir: PathBuf,
}

/// File in a cache dir holding the projects dir the cache is for
pub const SOURCE_MARKER: &str = "source";

/// Directory an earlier format kept its full-text index under; removed
/// on sight
const LEGACY_TEXT_INDEX_DIR: &str = "text";

impl IndexStore {
    pub fn new(root: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache_dir: cache_dir.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn lock_path(&self) -> PathBuf {
        self.cache_dir.join("reconcile.lock")
    }

    fn manifest_path(&self) -> PathBuf {
        self.cache_dir.join("manifest.json")
    }

    /// Read the cached `SessionSummary` for `id` if its file exists and
    /// its mtime is at least `jsonl_mtime`. Returns `None` on missing
    /// file, stale cache, or decode failure — the caller falls through
    /// to a full parse. Lock-free; the writer is reconcile, whose
    /// tmp+rename pattern means the reader either sees the old file or
    /// the new one, never a partial.
    pub fn session_summary(&self, id: &str, jsonl_mtime: SystemTime) -> Option<SessionSummary> {
        summary_cache::read(&self.cache_dir, id, jsonl_mtime)
    }

    /// Persist a `SessionSummary` to the on-disk cache. Used by the
    /// session-table lazy path to populate the cache as it derives
    /// sessions on demand, so subsequent queries hit `session_summary`.
    pub fn put_session_summary(&self, id: &str, summary: &SessionSummary) -> std::io::Result<()> {
        self.ensure_cache_dir()?;
        summary_cache::write(&self.cache_dir, id, summary)
    }

    /// Create the cache dir and record the projects dir it caches in
    /// its [`SOURCE_MARKER`] file. A sweep reads the marker to find
    /// cache dirs whose source no longer exists.
    pub fn ensure_cache_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)?;
        let marker = self.cache_dir.join(SOURCE_MARKER);
        if !marker.exists() {
            std::fs::write(marker, format!("{}\n", self.root.to_string_lossy()))?;
        }
        Ok(())
    }

    /// Run the reconcile pass: diff the corpus against the cache,
    /// re-derive changed sessions, garbage-collect removed ones.
    pub fn reconcile(&self, mode: LockMode) -> Result<ReconcileOutcome> {
        self.reconcile_with_progress(mode, |_| {})
    }

    /// As `reconcile`, but with a per-event progress callback.
    pub fn reconcile_with_progress(
        &self,
        mode: LockMode,
        mut on_event: impl FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        self.locked(mode, |store| store.reconcile_locked(&mut on_event))
    }

    /// Delete the cache and rebuild from scratch.
    pub fn rebuild(&self) -> Result<ReconcileOutcome> {
        self.rebuild_with_progress(|_| {})
    }

    /// As `rebuild`, but with a per-event progress callback.
    pub fn rebuild_with_progress(
        &self,
        mut on_event: impl FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        self.locked(LockMode::Wait, |store| {
            remove_file_if_present(&store.manifest_path())?;
            summary_cache::remove_all(&store.cache_dir)?;
            store.reconcile_locked(&mut on_event)
        })
    }

    fn locked(
        &self,
        mode: LockMode,
        body: impl FnOnce(&Self) -> Result<ReconcileOutcome>,
    ) -> Result<ReconcileOutcome> {
        self.ensure_cache_dir()?;
        self.remove_stale_artifacts();

        let process = process_lock(&self.cache_dir);
        let _process_guard = match mode {
            LockMode::Wait => process.lock().expect("reconcile process lock"),
            LockMode::Try => match process.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Ok(ReconcileOutcome {
                        skipped: true,
                        ..Default::default()
                    });
                }
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            },
        };

        let lock_file = File::create(self.lock_path())?;
        match mode {
            LockMode::Wait => {
                FileExt::lock(&lock_file)?;
            }
            LockMode::Try => match FileExt::try_lock(&lock_file) {
                Ok(()) => {}
                Err(fs4::TryLockError::WouldBlock) => {
                    return Ok(ReconcileOutcome {
                        skipped: true,
                        ..Default::default()
                    });
                }
                Err(fs4::TryLockError::Error(e)) => return Err(e.into()),
            },
        }
        // The OS releases the lock when `lock_file` drops — no stale
        // lock state survives a crash
        let result = body(self);
        #[allow(clippy::let_underscore_must_use)]
        let _ = FileExt::unlock(&lock_file);
        result
    }

    /// Remove summary-cache version directories that do not match the
    /// current code's version, and the full-text index an earlier
    /// format left behind.
    fn remove_stale_artifacts(&self) {
        let legacy = self.cache_dir.join(LEGACY_TEXT_INDEX_DIR);
        if legacy.is_dir() {
            tracing::info!(path = %legacy.display(), "removing legacy text index");
            if let Err(e) = std::fs::remove_dir_all(&legacy) {
                tracing::warn!(path = %legacy.display(), "failed to remove: {e}");
            }
        }
        summary_cache::remove_stale_versions(&self.cache_dir);
    }

    fn reconcile_locked(
        &self,
        on_event: &mut dyn FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        let start = std::time::Instant::now();

        // 1. Walk: (mtime, size) per session, no file reads.
        let walked: Vec<(String, PathBuf, Fingerprint)> = crate::session::SessionListBuilder::new()
            .root(&self.root)
            .build()
            .into_iter()
            .map(|s| {
                let mtime_ms = s
                    .mtime
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                (
                    s.id,
                    s.src,
                    Fingerprint {
                        mtime_ms,
                        size: s.size,
                    },
                )
            })
            .collect();
        let walked_ids: BTreeSet<&str> = walked.iter().map(|(id, _, _)| id.as_str()).collect();

        let mut manifest = load_manifest(&self.manifest_path());
        if !manifest.is_current() {
            tracing::info!("summary cache format changed, rebuilding");
            summary_cache::remove_all(&self.cache_dir)?;
            manifest = Manifest::empty();
        }

        // 2. Diff. A session is dirty if either the manifest doesn't
        // have it at the current fingerprint, or its summary cache
        // file is missing (covers first-run backfill and any
        // out-of-band deletion under `session/`).
        let cached = summary_cache::existing_ids(&self.cache_dir);
        let dirty: Vec<&(String, PathBuf, Fingerprint)> = walked
            .iter()
            .filter(|(id, _, fp)| manifest.sessions.get(id) != Some(fp) || !cached.contains(id))
            .collect();
        let removed: Vec<String> = manifest
            .sessions
            .keys()
            .filter(|id| !walked_ids.contains(id.as_str()))
            .cloned()
            .collect();

        let mut outcome = ReconcileOutcome {
            discovered: walked.len(),
            ..Default::default()
        };

        on_event(ReconcileEvent::Start {
            total: (dirty.len() + removed.len()) as u64,
        });

        // 3. Re-derive changed sessions: parse the JSONL and persist
        // the summary cache for the `session` table fast path. A
        // session whose summary fails to write stays out of the
        // manifest, so the next pass retries it.
        for (id, path, fp) in &dirty {
            let derived = match derive_session(id, path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(session_id = %id, "skipping unreadable session: {e}");
                    on_event(ReconcileEvent::Advance);
                    continue;
                }
            };
            if let Err(e) = summary_cache::write(&self.cache_dir, id, &derived.summary) {
                tracing::warn!(session_id = %id, "failed to write summary cache: {e}");
                on_event(ReconcileEvent::Advance);
                continue;
            }
            manifest.sessions.insert(id.clone(), *fp);
            outcome.indexed += 1;
            tracing::info!(session_id = %id, "indexed session");
            on_event(ReconcileEvent::Advance);
        }

        // 4. Garbage collection: summary cache files for sessions gone
        // from disk.
        for id in &removed {
            manifest.sessions.remove(id);
            if let Err(e) = summary_cache::remove(&self.cache_dir, id) {
                tracing::warn!(session_id = %id, "failed to remove summary cache: {e}");
            }
            outcome.removed += 1;
            on_event(ReconcileEvent::Advance);
        }

        // 5. Write the manifest.
        manifest.last_reconcile_ms = now_ms();
        write_manifest(&self.manifest_path(), &manifest)?;

        outcome.elapsed_ms = start.elapsed().as_millis() as u64;
        tracing::debug!(
            discovered = outcome.discovered,
            indexed = outcome.indexed,
            removed = outcome.removed,
            elapsed_ms = outcome.elapsed_ms,
            "reconcile complete",
        );
        Ok(outcome)
    }

    /// The status report: cache version, session counts, dirty count,
    /// on-disk size, last reconcile time. Read-only — takes no lock.
    pub fn status(&self) -> Status {
        let walked: HashMap<String, Fingerprint> = crate::session::SessionListBuilder::new()
            .root(&self.root)
            .build()
            .into_iter()
            .map(|s| {
                let mtime_ms = s
                    .mtime
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                (
                    s.id,
                    Fingerprint {
                        mtime_ms,
                        size: s.size,
                    },
                )
            })
            .collect();

        let manifest = load_manifest(&self.manifest_path());

        let dirty = walked
            .iter()
            .filter(|(id, fp)| manifest.sessions.get(id.as_str()) != Some(fp))
            .count();

        Status {
            cache_version: summary_cache::VERSION,
            discovered: walked.len(),
            indexed: manifest.sessions.len(),
            dirty,
            last_reconcile_ms: (manifest.last_reconcile_ms > 0)
                .then_some(manifest.last_reconcile_ms),
            cache_dir: self.cache_dir.clone(),
            cache_bytes: dir_size(&self.cache_dir),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn load_manifest(path: &Path) -> Manifest {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!("unreadable cache manifest, rebuilding: {e}");
            Manifest {
                format_version: 0,
                ..Manifest::empty()
            }
        }),
        Err(_) => Manifest::empty(),
    }
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(manifest)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}
