//! The reconcile pass and the `IndexStore` handle that owns the
//! tantivy text index.
//!
//! Any query of the message-text TVF or the session-row tables
//! reconciles first. Index writes are atomic at commit time; the pass
//! itself serializes on an advisory file lock. Queries try-lock and on
//! contention skip reconciliation, searching the current committed
//! snapshot — the contender is reconciling the same corpus, so
//! staleness is bounded at one pass and one-directional: missed
//! recent rows, never wrong rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::FileExt;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::derive::{Fingerprint, SessionSummary, derive_session};
use crate::summary_cache;
use crate::text_index::{Hit, INDEX_FORMAT_VERSION, TOKENIZER_CHAIN, TextIndex};

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
    /// reconcile lock; the index is at most one pass stale.
    pub skipped: bool,
    /// Sessions discovered on disk.
    pub discovered: usize,
    /// Sessions re-indexed.
    pub indexed: usize,
    /// Sessions removed from the index.
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
    pub index_version: u32,
    pub tokenizer_chain: String,
    pub discovered: usize,
    pub indexed: usize,
    pub dirty: usize,
    pub index_bytes: u64,
    pub last_reconcile_ms: Option<i64>,
    pub cache_dir: PathBuf,
    pub cache_bytes: u64,
}

impl Status {
    pub fn last_reconcile_display(&self) -> String {
        match self
            .last_reconcile_ms
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            None => "never".to_string(),
        }
    }

    pub fn index_bytes_display(&self) -> String {
        human_bytes(self.index_bytes)
    }

    pub fn cache_bytes_display(&self) -> String {
        human_bytes(self.cache_bytes)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "index:          v{} ({} sessions indexed, {}, tokenizer {})",
            self.index_version,
            self.indexed,
            self.index_bytes_display(),
            self.tokenizer_chain
        )?;
        writeln!(
            f,
            "sessions:       {} discovered, {} dirty",
            self.discovered, self.dirty
        )?;
        writeln!(f, "last reconcile: {}", self.last_reconcile_display())?;
        write!(
            f,
            "cache dir:      {} ({})",
            self.cache_dir.display(),
            self.cache_bytes_display(),
        )
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("GB"))
    }
}

/// Per-session index state, written only under the reconcile lock,
/// after the index commit. A crash between commit and manifest write
/// understates the index; the next reconcile re-indexes those
/// sessions, and delete-then-add is idempotent.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    tokenizer_chain: String,
    last_reconcile_ms: i64,
    sessions: BTreeMap<String, Fingerprint>,
}

impl Manifest {
    fn empty() -> Self {
        Self {
            format_version: INDEX_FORMAT_VERSION,
            tokenizer_chain: TOKENIZER_CHAIN.to_string(),
            last_reconcile_ms: 0,
            sessions: BTreeMap::new(),
        }
    }

    fn is_current(&self) -> bool {
        self.format_version == INDEX_FORMAT_VERSION && self.tokenizer_chain == TOKENIZER_CHAIN
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

/// Handle to the text index for one session corpus.
///
/// `root` is the Claude projects directory the corpus is read from;
/// `cache_dir` holds the index (`~/.gage/cache` in production).
/// Everything under `cache_dir` is rebuildable; deleting it is a
/// complete reset.
#[derive(Debug, Clone)]
pub struct IndexStore {
    root: PathBuf,
    cache_dir: PathBuf,
}

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

    fn index_dir(&self) -> PathBuf {
        self.cache_dir
            .join("text")
            .join(format!("v{INDEX_FORMAT_VERSION}"))
    }

    fn lock_path(&self) -> PathBuf {
        self.cache_dir.join("reconcile.lock")
    }

    fn manifest_path(&self) -> PathBuf {
        self.index_dir().join("manifest.json")
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
        summary_cache::write(&self.cache_dir, id, summary)
    }

    /// Run a query against the committed index, returning the top
    /// `limit` hits with BM25 scores and snippets capped at
    /// `snippet_chars`. An absent index matches nothing.
    pub fn search(&self, query: &str, limit: usize, snippet_chars: usize) -> Result<Vec<Hit>> {
        let dir = self.index_dir();
        if !dir.join("meta.json").exists() {
            return Ok(Vec::new());
        }
        TextIndex::open_or_create(&dir)?.search(query, limit, snippet_chars)
    }

    /// Run the reconcile pass: diff the corpus against the index,
    /// re-index changed sessions, garbage-collect removed ones.
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

    /// Delete the index and rebuild from scratch.
    pub fn rebuild(&self) -> Result<ReconcileOutcome> {
        self.rebuild_with_progress(|_| {})
    }

    /// As `rebuild`, but with a per-event progress callback.
    pub fn rebuild_with_progress(
        &self,
        mut on_event: impl FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        self.locked(LockMode::Wait, |store| {
            let index = store.index_dir();
            if index.exists() {
                std::fs::remove_dir_all(&index)?;
            }
            std::fs::create_dir_all(&index)?;
            summary_cache::remove_all(&store.cache_dir)?;
            store.reconcile_locked(&mut on_event)
        })
    }

    fn locked(
        &self,
        mode: LockMode,
        body: impl FnOnce(&Self) -> Result<ReconcileOutcome>,
    ) -> Result<ReconcileOutcome> {
        std::fs::create_dir_all(self.index_dir())?;
        self.remove_stale_versions();

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

    /// Remove version directories under `text/` and `session/` whose
    /// `v{N}` doesn't match the current code's version.
    fn remove_stale_versions(&self) {
        let parent = self.cache_dir.join("text");
        let current = format!("v{INDEX_FORMAT_VERSION}");
        let entries = match std::fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && entry.file_name().to_string_lossy() != current {
                tracing::info!(path = %path.display(), "removing stale text-index version");
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(path = %path.display(), "failed to remove: {e}");
                }
            }
        }
        summary_cache::remove_stale_versions(&self.cache_dir);
    }

    fn reconcile_locked(
        &self,
        on_event: &mut dyn FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        let start = std::time::Instant::now();
        let index_dir = self.index_dir();

        // 1. Walk: (mtime, size) per session, no file reads.
        let walked: Vec<(String, PathBuf, Fingerprint)> =
            gage_claude::session::SessionListBuilder::new()
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
        let index = match TextIndex::open_or_create(&index_dir) {
            Ok(index) if manifest.is_current() => index,
            other => {
                // Version/chain mismatch or unreadable index: wipe and
                // rebuild.
                if let Err(e) = other {
                    tracing::warn!("text index unreadable, rebuilding: {e}");
                } else {
                    tracing::info!("text index format changed, rebuilding");
                }
                std::fs::remove_dir_all(&index_dir)?;
                std::fs::create_dir_all(&index_dir)?;
                manifest = Manifest::empty();
                TextIndex::open_or_create(&index_dir)?
            }
        };

        // 2. Diff. A session is dirty if either the text-index manifest
        // doesn't have it at the current fingerprint, or its summary
        // cache file is missing (covers first-run backfill and any
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

        let work = !dirty.is_empty() || !removed.is_empty();
        let mut writer = if work { Some(index.writer()?) } else { None };

        on_event(ReconcileEvent::Start {
            total: (dirty.len() + removed.len()) as u64,
        });

        // 3. Re-index changed sessions: parse the JSONL, write each
        // message line into the text index, and persist the summary
        // cache for the `session` table fast path.
        for (id, path, fp) in &dirty {
            let derived = match derive_session(id, path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(session_id = %id, "skipping unreadable session: {e}");
                    on_event(ReconcileEvent::Advance);
                    continue;
                }
            };
            if let Some(w) = &writer {
                index.delete_session(w, id);
                for (line, type_, subtype, text) in message_rows_of(&derived.batch) {
                    index.add_message(w, id, line, type_, subtype, text)?;
                }
                manifest.sessions.insert(id.clone(), *fp);
            }
            if let Err(e) = summary_cache::write(&self.cache_dir, id, &derived.summary) {
                tracing::warn!(session_id = %id, "failed to write summary cache: {e}");
            }
            outcome.indexed += 1;
            tracing::info!(session_id = %id, "indexed session");
            on_event(ReconcileEvent::Advance);
        }

        // 4. Garbage collection: index entries and summary cache files
        // for sessions gone from disk.
        if let Some(w) = &writer {
            for id in &removed {
                index.delete_session(w, id);
                manifest.sessions.remove(id);
                if let Err(e) = summary_cache::remove(&self.cache_dir, id) {
                    tracing::warn!(session_id = %id, "failed to remove summary cache: {e}");
                }
                outcome.removed += 1;
                on_event(ReconcileEvent::Advance);
            }
        }

        // 5. Commit, then write the manifest.
        if let Some(w) = &mut writer {
            w.commit()?;
        }
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

    /// The status report: format version, session counts, dirty
    /// count, on-disk sizes, last reconcile time. Read-only — takes
    /// no lock.
    pub fn status(&self) -> Status {
        let walked: HashMap<String, Fingerprint> = gage_claude::session::SessionListBuilder::new()
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
            index_version: INDEX_FORMAT_VERSION,
            tokenizer_chain: TOKENIZER_CHAIN.to_string(),
            discovered: walked.len(),
            indexed: manifest.sessions.len(),
            dirty,
            index_bytes: dir_size(&self.index_dir()),
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
            tracing::warn!("unreadable index manifest, rebuilding: {e}");
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

/// Iterate `(line, text)` over the message rows of a derived batch.
fn message_rows_of(
    batch: &arrow::record_batch::RecordBatch,
) -> Vec<(i64, Option<&str>, Option<&str>, &str)> {
    use arrow::array::{Array, Int64Array, StringArray};

    use crate::derive::{COL_LINE, COL_SUBTYPE, COL_TEXT, COL_TYPE};

    let lines = batch
        .columns()
        .get(COL_LINE)
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let types = batch
        .columns()
        .get(COL_TYPE)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let subtypes = batch
        .columns()
        .get(COL_SUBTYPE)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let texts = batch
        .columns()
        .get(COL_TEXT)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let (Some(lines), Some(types), Some(subtypes), Some(texts)) = (lines, types, subtypes, texts)
    else {
        return Vec::new();
    };
    (0..batch.num_rows())
        .filter(|&i| texts.is_valid(i))
        .map(|i| {
            (
                lines.value(i),
                types.is_valid(i).then(|| types.value(i)),
                subtypes.is_valid(i).then(|| subtypes.value(i)),
                texts.value(i),
            )
        })
        .collect()
}
