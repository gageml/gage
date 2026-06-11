//! The reconcile pass and the `IndexStore` handle that owns the
//! derived artifacts.
//!
//! Any query of the session-file tables reconciles first, then scans
//! derived artifacts only. Cache writes are lock-free (temp + atomic
//! rename, deterministic content); the pass itself serializes on an
//! advisory file lock. Queries try-lock and on contention skip
//! reconciliation, searching the current committed snapshot — the
//! contender is reconciling the same corpus, so staleness is bounded
//! at one pass and is one-directional: missed recent rows, never
//! wrong rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::UNIX_EPOCH;

use fs4::FileExt;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::derive::{Fingerprint, SessionAggregates, derive_session};
use crate::store::{
    STORE_FORMAT_VERSION, read_aggregates, read_fingerprint, read_message_rows, write_aggregates,
    write_session_file,
};
use crate::text_index::{INDEX_FORMAT_VERSION, TOKENIZER_CHAIN, TextIndex};

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
    /// reconcile lock; artifacts are at most one pass stale.
    pub skipped: bool,
    /// Sessions discovered on disk.
    pub discovered: usize,
    /// Sessions re-derived (JSONL parsed, store file rewritten).
    pub derived: usize,
    /// Sessions re-indexed from the store without re-deriving.
    pub reindexed: usize,
    /// Sessions removed from the artifacts.
    pub removed: usize,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub store_version: u32,
    pub index_version: u32,
    pub tokenizer_chain: String,
    pub discovered: usize,
    pub cached: usize,
    pub indexed: usize,
    pub dirty: usize,
    pub store_bytes: u64,
    pub index_bytes: u64,
    pub last_reconcile_ms: Option<i64>,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "store:          v{} ({} sessions cached, {})",
            self.store_version,
            self.cached,
            human_bytes(self.store_bytes)
        )?;
        writeln!(
            f,
            "index:          v{} ({} sessions indexed, {}, tokenizer {})",
            self.index_version,
            self.indexed,
            human_bytes(self.index_bytes),
            self.tokenizer_chain
        )?;
        writeln!(
            f,
            "sessions:       {} discovered, {} dirty",
            self.discovered, self.dirty
        )?;
        let last = match self
            .last_reconcile_ms
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            None => "never".to_string(),
        };
        write!(f, "last reconcile: {last}")
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

/// A cache file's own identity: `(mtime_ns, size)`. Session files are
/// only ever replaced whole (temp + atomic rename), so a matching
/// stat means matching content.
type FileStat = (i128, u64);

/// Memoized footer fingerprints, keyed by cache-file path and
/// validated by the file's stat. Spares each reconcile pass from
/// re-opening every session file's footer; a replaced file has a new
/// stat and re-reads.
type FooterCache = Mutex<HashMap<PathBuf, (FileStat, Option<Fingerprint>)>>;

/// Handle to the derived artifacts for one session corpus.
///
/// `root` is the Claude projects directory the corpus is read from;
/// `cache_dir` holds the artifacts (`~/.gage/cache` in production).
/// Everything under `cache_dir` is rebuildable; deleting it is a
/// complete reset.
#[derive(Debug, Clone)]
pub struct IndexStore {
    root: PathBuf,
    cache_dir: PathBuf,
    footer_cache: Arc<FooterCache>,
}

impl IndexStore {
    pub fn new(root: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache_dir: cache_dir.into(),
            footer_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The source fingerprint recorded in a session file's footer,
    /// memoized on the file's own stat.
    fn footer_fingerprint(&self, path: &Path) -> Option<Fingerprint> {
        let meta = path.metadata().ok()?;
        let stat: FileStat = (
            meta.modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i128,
            meta.len(),
        );
        let mut cache = self.footer_cache.lock().expect("footer cache");
        if let Some((cached_stat, fp)) = cache.get(path)
            && *cached_stat == stat
        {
            return *fp;
        }
        let fp = read_fingerprint(path);
        cache.insert(path.to_path_buf(), (stat, fp));
        fp
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.cache_dir
            .join("sessions")
            .join(format!("v{STORE_FORMAT_VERSION}"))
    }

    fn index_dir(&self) -> PathBuf {
        self.cache_dir
            .join("text-index")
            .join(format!("v{INDEX_FORMAT_VERSION}"))
    }

    fn lock_path(&self) -> PathBuf {
        self.cache_dir.join("reconcile.lock")
    }

    fn manifest_path(&self) -> PathBuf {
        self.index_dir().join("manifest.json")
    }

    fn aggregates_path(&self) -> PathBuf {
        self.sessions_dir().join("sessions.parquet")
    }

    /// Path of one session's store file.
    pub fn session_file(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{session_id}.parquet"))
    }

    /// List `(session_id, path)` for every session file in the store.
    pub fn session_files(&self) -> Vec<(String, PathBuf)> {
        let mut files = Vec::new();
        let entries = match std::fs::read_dir(self.sessions_dir()) {
            Ok(entries) => entries,
            Err(_) => return files,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if let Some(id) = name.strip_suffix(".parquet")
                && is_session_id(id)
            {
                files.push((id.to_string(), path));
            }
        }
        files.sort();
        files
    }

    /// Load the consolidated session aggregates.
    pub fn load_aggregates(&self) -> Result<HashMap<String, SessionAggregates>> {
        read_aggregates(&self.aggregates_path())
    }

    /// Search the committed text index, returning matched
    /// `(session_id, line)` coordinates. An absent index matches
    /// nothing. Query syntax errors are reported as
    /// [`IndexError::QueryParse`].
    pub fn search(&self, query: &str) -> Result<Vec<(String, i64)>> {
        let dir = self.index_dir();
        if !dir.join("meta.json").exists() {
            return Ok(Vec::new());
        }
        TextIndex::open_or_create(&dir)?.search(query)
    }

    /// Run the reconcile pass: diff the corpus against the artifacts,
    /// re-derive and re-index changed sessions, garbage-collect
    /// removed ones.
    pub fn reconcile(&self, mode: LockMode) -> Result<ReconcileOutcome> {
        self.locked(mode, |store| store.reconcile_locked())
    }

    /// Delete both artifacts and rebuild from scratch.
    pub fn rebuild(&self) -> Result<ReconcileOutcome> {
        self.locked(LockMode::Wait, |store| {
            let sessions = store.sessions_dir();
            if sessions.exists() {
                std::fs::remove_dir_all(&sessions)?;
            }
            let index = store.index_dir();
            if index.exists() {
                std::fs::remove_dir_all(&index)?;
            }
            std::fs::create_dir_all(&sessions)?;
            std::fs::create_dir_all(&index)?;
            store.reconcile_locked()
        })
    }

    fn locked(
        &self,
        mode: LockMode,
        body: impl FnOnce(&Self) -> Result<ReconcileOutcome>,
    ) -> Result<ReconcileOutcome> {
        std::fs::create_dir_all(self.sessions_dir())?;
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

    /// Remove version directories other than the current formats'.
    fn remove_stale_versions(&self) {
        for (parent, current) in [
            (
                self.cache_dir.join("sessions"),
                format!("v{STORE_FORMAT_VERSION}"),
            ),
            (
                self.cache_dir.join("text-index"),
                format!("v{INDEX_FORMAT_VERSION}"),
            ),
        ] {
            let entries = match std::fs::read_dir(&parent) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && entry.file_name().to_string_lossy() != current {
                    tracing::info!(path = %path.display(), "removing stale format version");
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        tracing::warn!(path = %path.display(), "failed to remove: {e}");
                    }
                }
            }
        }
    }

    fn reconcile_locked(&self) -> Result<ReconcileOutcome> {
        let start = std::time::Instant::now();
        let sessions_dir = self.sessions_dir();
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

        // 2. Current artifact state.
        let cached: HashMap<String, Option<Fingerprint>> = self
            .session_files()
            .into_iter()
            .map(|(id, path)| {
                let fp = self.footer_fingerprint(&path);
                (id, fp)
            })
            .collect();

        let mut manifest = load_manifest(&self.manifest_path());
        let index = match TextIndex::open_or_create(&index_dir) {
            Ok(index) if manifest.is_current() => index,
            other => {
                // Version/chain mismatch or unreadable index: wipe and
                // rebuild. Recovery is a column scan over the store.
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

        let mut aggregates: BTreeMap<String, SessionAggregates> = self
            .load_aggregates()
            .unwrap_or_default()
            .into_iter()
            .collect();

        // 3. Diff. A session missing its aggregates row re-derives —
        // aggregates come from fields (usage, ai-title) that only the
        // JSONL parse sees.
        let store_dirty: Vec<&(String, PathBuf, Fingerprint)> = walked
            .iter()
            .filter(|(id, _, fp)| {
                cached.get(id) != Some(&Some(*fp)) || !aggregates.contains_key(id)
            })
            .collect();
        let index_dirty: Vec<&(String, PathBuf, Fingerprint)> = walked
            .iter()
            .filter(|(id, _, fp)| manifest.sessions.get(id) != Some(fp))
            .collect();
        let store_removed: Vec<String> = cached
            .keys()
            .filter(|id| !walked_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let index_removed: Vec<String> = manifest
            .sessions
            .keys()
            .filter(|id| !walked_ids.contains(id.as_str()))
            .cloned()
            .collect();

        let mut outcome = ReconcileOutcome {
            discovered: walked.len(),
            ..Default::default()
        };

        let work = !store_dirty.is_empty()
            || !index_dirty.is_empty()
            || !store_removed.is_empty()
            || !index_removed.is_empty();

        let mut writer = if index_dirty.is_empty() && index_removed.is_empty() {
            None
        } else {
            Some(index.writer()?)
        };

        // 4. Re-derive changed sessions: parse the JSONL once, write
        // the store file (rename into place before its documents are
        // added to the index), then re-index.
        let store_dirty_ids: BTreeSet<&str> =
            store_dirty.iter().map(|(id, _, _)| id.as_str()).collect();
        for (id, path, _) in &store_dirty {
            let derived = match derive_session(id, path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(session_id = %id, "skipping unreadable session: {e}");
                    continue;
                }
            };
            write_session_file(&sessions_dir, &derived)?;
            aggregates.insert(id.clone(), derived.aggregates.clone());
            if let Some(w) = &writer {
                index.delete_session(w, id);
                for (line, text) in message_rows_of(&derived.batch) {
                    index.add_message(w, id, line, text)?;
                }
                manifest.sessions.insert(id.clone(), derived.fingerprint);
            }
            outcome.derived += 1;
            tracing::info!(session_id = %id, "derived session");
        }

        // 5. Index-only refresh (e.g. after an index rebuild): read
        // the text column from the store instead of re-parsing JSONL.
        for (id, _, _) in &index_dirty {
            if store_dirty_ids.contains(id.as_str()) {
                continue;
            }
            let Some(Some(fp)) = cached.get(id) else {
                continue;
            };
            let rows = match read_message_rows(&self.session_file(id)) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(session_id = %id, "skipping unreadable store file: {e}");
                    continue;
                }
            };
            if let Some(w) = &writer {
                index.delete_session(w, id);
                for (line, text) in &rows {
                    index.add_message(w, id, *line, text)?;
                }
                manifest.sessions.insert(id.clone(), *fp);
            }
            outcome.reindexed += 1;
        }

        // 6. Garbage collection: artifacts for sessions gone from
        // disk. Stale index entries cannot produce wrong results —
        // re-application and absent rows see to that.
        for id in &store_removed {
            let path = self.session_file(id);
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(session_id = %id, "failed to remove store file: {e}");
            }
            #[allow(clippy::let_underscore_must_use)]
            let _ = self
                .footer_cache
                .lock()
                .expect("footer cache")
                .remove(&path);
            outcome.removed += 1;
        }
        aggregates.retain(|id, _| walked_ids.contains(id.as_str()));
        if let Some(w) = &writer {
            for id in &index_removed {
                index.delete_session(w, id);
                manifest.sessions.remove(id);
            }
        }

        // 7. Commit ordering: all cache writes above precede the
        // index commit; the manifest is written last.
        if let Some(w) = &mut writer {
            w.commit()?;
        }
        if work {
            write_aggregates(&self.aggregates_path(), &aggregates)?;
        }
        manifest.last_reconcile_ms = now_ms();
        write_manifest(&self.manifest_path(), &manifest)?;

        tracing::debug!(
            discovered = outcome.discovered,
            derived = outcome.derived,
            reindexed = outcome.reindexed,
            removed = outcome.removed,
            elapsed_ms = start.elapsed().as_millis(),
            "reconcile complete",
        );
        Ok(outcome)
    }

    /// The status report: format versions, session counts, dirty
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

        let cached: HashMap<String, Option<Fingerprint>> = self
            .session_files()
            .into_iter()
            .map(|(id, path)| (id, self.footer_fingerprint(&path)))
            .collect();
        let manifest = load_manifest(&self.manifest_path());

        let dirty = walked
            .iter()
            .filter(|(id, fp)| {
                cached.get(id.as_str()) != Some(&Some(**fp))
                    || manifest.sessions.get(id.as_str()) != Some(fp)
            })
            .count();

        Status {
            store_version: STORE_FORMAT_VERSION,
            index_version: INDEX_FORMAT_VERSION,
            tokenizer_chain: TOKENIZER_CHAIN.to_string(),
            discovered: walked.len(),
            cached: cached.len(),
            indexed: manifest.sessions.len(),
            dirty,
            store_bytes: dir_size(&self.sessions_dir()),
            index_bytes: dir_size(&self.index_dir()),
            last_reconcile_ms: (manifest.last_reconcile_ms > 0)
                .then_some(manifest.last_reconcile_ms),
        }
    }
}

fn is_session_id(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
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
fn message_rows_of(batch: &arrow::record_batch::RecordBatch) -> Vec<(i64, &str)> {
    use arrow::array::{Array, Int64Array, StringArray};

    use crate::derive::{COL_LINE, COL_TEXT};

    let lines = batch
        .columns()
        .get(COL_LINE)
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let texts = batch
        .columns()
        .get(COL_TEXT)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let (Some(lines), Some(texts)) = (lines, texts) else {
        return Vec::new();
    };
    (0..batch.num_rows())
        .filter(|&i| texts.is_valid(i))
        .map(|i| (lines.value(i), texts.value(i)))
        .collect()
}
