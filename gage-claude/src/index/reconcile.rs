//! The reconcile pass and the `IndexStore` handle that owns the
//! per-session summary cache.
//!
//! The cache of one corpus is the directory `<cache root>/summary/<slug>`,
//! where the slug encodes the corpus's projects directory. It holds
//! `manifest.json`, naming the format version, the source, and the
//! fingerprint of every session indexed; the reconcile lock; and one
//! summary file per session. The directory is validated when the
//! handle's directory is ensured: a manifest of another format version,
//! or none, discards the directory and starts it empty.
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

use arrow::array::{Array, Int64Array, StringArray};
use arrow::record_batch::RecordBatch;
use fs4::FileExt;
use gage_index::{LockMode, MessageRow, TextIndex, corpus_slug};
use gage_session::MessageTextHit;
use serde::{Deserialize, Serialize};

use super::Result;
use super::derive::{
    COL_LINE, COL_MESSAGE_SUBTYPE, COL_TEXT, COL_TYPE, ClaudeSessionAttrs, Fingerprint,
    derive_session,
};
use super::summary_cache;
use crate::session::SessionListBuilder;

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
    pub summary_dir: PathBuf,
    pub cache_bytes: u64,
    pub text_dir: PathBuf,
    pub text_bytes: u64,
}

/// The directory's record: the format its files are in, the projects
/// directory it caches, and the per-session state. The session map is
/// written only under the reconcile lock, after the summaries are
/// written. A crash before the manifest write understates the cache;
/// the next reconcile re-derives those sessions, and the summary write
/// is idempotent.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    source: PathBuf,
    last_reconcile_ms: i64,
    sessions: BTreeMap<String, Fingerprint>,
}

impl Manifest {
    fn empty(source: &Path) -> Self {
        Self {
            format_version: summary_cache::VERSION,
            source: source.to_path_buf(),
            last_reconcile_ms: 0,
            sessions: BTreeMap::new(),
        }
    }

    fn is_current(&self) -> bool {
        self.format_version == summary_cache::VERSION
    }
}

/// In-process serialization of reconcile passes, keyed by summary dir.
/// The file lock serializes across processes; this mutex keeps
/// concurrent scans within one process from duplicating work (and
/// from try-lock-failing against their own siblings).
static PROCESS_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn process_lock(summary_dir: &Path) -> Arc<Mutex<()>> {
    let mut map = PROCESS_LOCKS.lock().expect("process lock registry");
    map.entry(summary_dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Handle to the derived artifacts of one session corpus: the summary
/// cache and the full-text index.
///
/// `root` is the Claude projects directory the corpus is read from;
/// the summary cache is the directory `summary/<slug>` and the text
/// index `text/<slug>` under `cache_root` (`~/.gage/cache` in
/// production). Everything under either is rebuildable; deleting it
/// is a complete reset.
#[derive(Debug, Clone)]
pub struct IndexStore {
    root: PathBuf,
    cache_root: PathBuf,
    slug: String,
    /// The source URL the text index records and prefixes session
    /// URLs with: `claude:<Claude root>`
    source_url: String,
    summary_dir: PathBuf,
    show_agents: bool,
}

/// The category directory under the cache root holding the summary
/// cache of every corpus
const SUMMARY_DIR: &str = "summary";

/// The scheme of a Claude session URL
const URL_SCHEME: &str = "claude";

impl IndexStore {
    pub fn new(root: impl Into<PathBuf>, cache_root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let cache_root = cache_root.into();
        let slug = corpus_slug(&root);
        let claude_root = root.parent().unwrap_or(&root);
        let source_url = format!("{URL_SCHEME}:{}", claude_root.display());
        let summary_dir = cache_root.join(SUMMARY_DIR).join(&slug);
        Self {
            root,
            cache_root,
            slug,
            source_url,
            summary_dir,
            show_agents: false,
        }
    }

    /// Include the runtime's own in-flight agent sessions in every
    /// walk of the projects directory
    pub fn show_agents(mut self, show: bool) -> Self {
        self.show_agents = show;
        self
    }

    pub fn shows_agents(&self) -> bool {
        self.show_agents
    }

    /// A session walk over this store's projects directory, reading
    /// agent sessions as the store does
    pub fn session_list(&self) -> SessionListBuilder {
        SessionListBuilder::new()
            .root(&self.root)
            .show_agents(self.show_agents)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding this corpus's manifest, lock, and
    /// summary files.
    pub fn summary_dir(&self) -> &Path {
        &self.summary_dir
    }

    /// The directory holding this corpus's full-text index.
    pub fn text_dir(&self) -> PathBuf {
        TextIndex::dir_for(&self.cache_root, &self.slug)
    }

    fn text_index(&self) -> Result<TextIndex> {
        Ok(TextIndex::open(
            &self.cache_root,
            &self.slug,
            &self.source_url,
        )?)
    }

    /// The URL of the session `id` in this corpus, as the text index
    /// carries it.
    fn session_url(&self, id: &str) -> String {
        format!("{}/{id}", self.source_url)
    }

    /// The top `limit` text-index hits for `query`, best match first,
    /// with snippets of at most `snippet_chars` characters. An index
    /// not yet built matches nothing.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        snippet_chars: usize,
    ) -> Result<Vec<MessageTextHit>> {
        Ok(self.text_index()?.search(query, limit, snippet_chars)?)
    }

    fn lock_path(&self) -> PathBuf {
        self.summary_dir.join(summary_cache::LOCK_FILE)
    }

    fn manifest_path(&self) -> PathBuf {
        self.summary_dir.join(summary_cache::MANIFEST_FILE)
    }

    /// Read the cached `ClaudeSessionAttrs` for `id` if its file exists and
    /// its mtime is at least `jsonl_mtime`. Returns `None` on missing
    /// file, stale cache, or decode failure — the caller falls through
    /// to a full parse. Lock-free; the writer is reconcile, whose
    /// tmp+rename pattern means the reader either sees the old file or
    /// the new one, never a partial.
    pub fn session_attrs(&self, id: &str, jsonl_mtime: SystemTime) -> Option<ClaudeSessionAttrs> {
        summary_cache::read(&self.summary_dir, id, jsonl_mtime)
    }

    /// Persist a `ClaudeSessionAttrs` to the on-disk cache. Used by the
    /// session-table lazy path to populate the cache as it derives
    /// sessions on demand, so subsequent queries hit `session_attrs`.
    pub fn put_session_attrs(&self, id: &str, attrs: &ClaudeSessionAttrs) -> std::io::Result<()> {
        self.ensure_summary_dir()?;
        summary_cache::write(&self.summary_dir, id, attrs)
    }

    /// Bring the summary directory to a usable state: created, with a
    /// manifest of the current format naming this corpus. A directory
    /// whose manifest is of another format, unreadable, or missing is
    /// discarded first; its files are of unknown shape.
    pub fn ensure_summary_dir(&self) -> std::io::Result<()> {
        let manifest_path = self.manifest_path();
        if self.summary_dir.is_dir() {
            match read_manifest(&manifest_path) {
                Ok(manifest) if manifest.is_current() => return Ok(()),
                Ok(_) => tracing::info!(
                    dir = %self.summary_dir.display(),
                    "summary cache format changed, discarding"
                ),
                Err(e) => tracing::info!(
                    dir = %self.summary_dir.display(),
                    "summary cache manifest unreadable, discarding: {e}"
                ),
            }
            std::fs::remove_dir_all(&self.summary_dir)?;
        }
        std::fs::create_dir_all(&self.summary_dir)?;
        write_manifest(&manifest_path, &Manifest::empty(&self.root))
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

    /// Delete the summary cache and the text index and rebuild both
    /// from scratch.
    pub fn rebuild(&self) -> Result<ReconcileOutcome> {
        self.rebuild_with_progress(|_| {})
    }

    /// As `rebuild`, but with a per-event progress callback.
    pub fn rebuild_with_progress(
        &self,
        mut on_event: impl FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        self.locked(LockMode::Wait, |store| {
            write_manifest(&store.manifest_path(), &Manifest::empty(&store.root))?;
            summary_cache::remove_all(&store.summary_dir)?;
            TextIndex::remove(&store.cache_root, &store.slug)?;
            store.reconcile_locked(&mut on_event)
        })
    }

    fn locked(
        &self,
        mode: LockMode,
        body: impl FnOnce(&Self) -> Result<ReconcileOutcome>,
    ) -> Result<ReconcileOutcome> {
        self.ensure_summary_dir()?;

        let process = process_lock(&self.summary_dir);
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

    fn reconcile_locked(
        &self,
        on_event: &mut dyn FnMut(ReconcileEvent),
    ) -> Result<ReconcileOutcome> {
        let start = std::time::Instant::now();

        // 1. Walk: (mtime, size) per session, no file reads.
        let walked: Vec<(String, PathBuf, Fingerprint)> = self
            .session_list()
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

        let mut manifest = load_manifest(&self.manifest_path(), &self.root);
        if !manifest.is_current() {
            tracing::info!("summary cache format changed, rebuilding");
            summary_cache::remove_all(&self.summary_dir)?;
            manifest = Manifest::empty(&self.root);
        }
        let mut text = self.text_index()?;

        // 2. Diff. A session is dirty if the manifest or the text index
        // lacks it at the current fingerprint, or its summary cache
        // file is missing (covers first-run backfill and any
        // out-of-band deletion of a summary file).
        let cached = summary_cache::existing_ids(&self.summary_dir);
        let dirty: Vec<&(String, PathBuf, Fingerprint)> = walked
            .iter()
            .filter(|(id, _, fp)| {
                manifest.sessions.get(id) != Some(fp)
                    || !cached.contains(id)
                    || text.sessions().get(id) != Some(&text_fingerprint(fp))
            })
            .collect();
        // A cache file with no manifest entry was derived by a table
        // read, as for an agent session read while it ran; once its
        // JSONL is gone it is collected with the rest.
        let removed: BTreeSet<String> = manifest
            .sessions
            .keys()
            .chain(cached.iter())
            .chain(text.sessions().keys())
            .filter(|id| !walked_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let mut writer = text.writer()?;

        let mut outcome = ReconcileOutcome {
            discovered: walked.len(),
            ..Default::default()
        };

        on_event(ReconcileEvent::Start {
            total: (dirty.len() + removed.len()) as u64,
        });

        // 3. Re-derive changed sessions: parse the JSONL once, persist
        // the summary cache for the `session` table fast path, and
        // replace the session's documents in the text index. A session
        // whose summary or documents fail to write stays out of its
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
            if let Err(e) = summary_cache::write(&self.summary_dir, id, &derived.attrs) {
                tracing::warn!(session_id = %id, "failed to write summary cache: {e}");
                on_event(ReconcileEvent::Advance);
                continue;
            }
            manifest.sessions.insert(id.clone(), *fp);
            if let Err(e) = writer.replace_session(
                id,
                &self.session_url(id),
                &text_fingerprint(fp),
                message_rows_of(&derived.batch),
            ) {
                tracing::warn!(session_id = %id, "failed to index message text: {e}");
                on_event(ReconcileEvent::Advance);
                continue;
            }
            outcome.indexed += 1;
            tracing::info!(session_id = %id, "indexed session");
            on_event(ReconcileEvent::Advance);
        }

        // 4. Garbage collection: summary cache files and text documents
        // for sessions gone from disk
        for id in &removed {
            manifest.sessions.remove(id);
            if let Err(e) = summary_cache::remove(&self.summary_dir, id) {
                tracing::warn!(session_id = %id, "failed to remove summary cache: {e}");
            }
            writer.remove_session(id);
            outcome.removed += 1;
            on_event(ReconcileEvent::Advance);
        }

        // 5. Commit the text index, then write the manifest.
        writer.commit()?;
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
        let walked: HashMap<String, Fingerprint> = self
            .session_list()
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

        let manifest = load_manifest(&self.manifest_path(), &self.root);

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
            summary_dir: self.summary_dir.clone(),
            cache_bytes: dir_size(&self.summary_dir),
            text_dir: self.text_dir(),
            text_bytes: dir_size(&self.text_dir()),
        }
    }
}

/// The fingerprint as the text index records it.
fn text_fingerprint(fp: &Fingerprint) -> String {
    format!("{}:{}", fp.mtime_ms, fp.size)
}

/// The message rows of a derived batch: the rows with text, each with
/// its line, entry type, and message subtype.
fn message_rows_of(batch: &RecordBatch) -> Vec<MessageRow> {
    let column = |i: usize| batch.columns().get(i);
    let (Some(lines), Some(types), Some(subtypes), Some(texts)) = (
        column(COL_LINE).and_then(|c| c.as_any().downcast_ref::<Int64Array>()),
        column(COL_TYPE).and_then(|c| c.as_any().downcast_ref::<StringArray>()),
        column(COL_MESSAGE_SUBTYPE).and_then(|c| c.as_any().downcast_ref::<StringArray>()),
        column(COL_TEXT).and_then(|c| c.as_any().downcast_ref::<StringArray>()),
    ) else {
        return Vec::new();
    };
    (0..batch.num_rows())
        .filter(|&i| texts.is_valid(i))
        .map(|i| MessageRow {
            line: lines.value(i),
            type_: types.is_valid(i).then(|| types.value(i).to_string()),
            subtype: subtypes.is_valid(i).then(|| subtypes.value(i).to_string()),
            text: texts.value(i).to_string(),
        })
        .collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// The manifest as written, or an empty one for `source` when the
/// file is absent. An unreadable file is reported as version 0, so
/// the caller rebuilds.
fn load_manifest(path: &Path, source: &Path) -> Manifest {
    match read_manifest(path) {
        Ok(manifest) => manifest,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest::empty(source),
        Err(e) => {
            tracing::warn!("unreadable cache manifest, rebuilding: {e}");
            Manifest {
                format_version: 0,
                ..Manifest::empty(source)
            }
        }
    }
}

fn read_manifest(path: &Path) -> std::io::Result<Manifest> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(std::io::Error::other)
}

fn write_manifest(path: &Path, manifest: &Manifest) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(manifest)?)?;
    std::fs::rename(&tmp, path)
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
