//! In-memory cache of derived sessions, keyed by `session_id` and
//! validated against the source file's `(mtime, size)` fingerprint.
//!
//! One cache serves the process. The `message`, `session`, `entry`,
//! and `tool_use` table providers all read through it, across every
//! source handle and every query — first touch of a session parses
//! the JSONL into a `DerivedSession`; subsequent touches stat the file
//! and either reuse the cached value or re-parse on fingerprint
//! mismatch. A session id names one file within a source, and the
//! fingerprint check makes a same-id file from another source a
//! re-parse rather than a wrong answer.
//!
//! Residency is bounded by [`CACHE_BYTES`] of Arrow batch memory. A
//! scan over the whole corpus holds the sessions in flight plus the
//! bound, not the corpus: without the bound a full scan of 4,300
//! sessions held 2.4 GB. Eviction is least recently used; a session
//! larger than the bound is held alone. An evicted session a stream
//! still reads stays alive through the stream's own `Arc`.
//!
//! Concurrency: a `tokio::sync::OnceCell` per slot keeps concurrent
//! first-touchers from duplicating the parse.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use crate::index::{DerivedSession, Fingerprint, derive_session};
use datafusion::error::{DataFusionError, Result};
use tokio::sync::OnceCell;

/// Resident bytes of derived sessions the process cache keeps
pub const CACHE_BYTES: usize = 256 * 1024 * 1024;

type Slot = Arc<OnceCell<Arc<DerivedSession>>>;

pub struct SessionCache {
    inner: Mutex<Inner>,
    loaded: AtomicUsize,
    bound: usize,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<String, Slot>,
    /// Filled slots with their batch bytes, least recently used first
    order: VecDeque<(String, usize)>,
    resident: usize,
}

impl Default for SessionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SessionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("SessionCache")
            .field("cached_sessions", &inner.slots.len())
            .field("resident_bytes", &inner.resident)
            .field("bound", &self.bound)
            .finish()
    }
}

static SHARED: LazyLock<Arc<SessionCache>> = LazyLock::new(|| Arc::new(SessionCache::new()));

impl SessionCache {
    pub fn new() -> Self {
        Self::with_bound(CACHE_BYTES)
    }

    /// A cache that keeps at most `bytes` of derived sessions resident.
    pub fn with_bound(bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            loaded: AtomicUsize::new(0),
            bound: bytes,
        }
    }

    /// The process's one cache, which every table provider reads
    /// through.
    pub fn shared() -> Arc<SessionCache> {
        Arc::clone(&SHARED)
    }

    /// Number of sessions whose JSONL has been parsed into a
    /// `DerivedSession`. Monotonically non-decreasing across the
    /// cache's lifetime. Lock-free.
    pub fn loaded(&self) -> usize {
        self.loaded.load(Ordering::Relaxed)
    }

    /// Arrow batch bytes of the sessions held, which eviction keeps
    /// at or under the bound unless one session alone exceeds it.
    pub fn resident_bytes(&self) -> usize {
        self.lock().resident
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
            let loaded = &self.loaded;
            let cached = slot
                .get_or_try_init(|| async move {
                    let derived = parse(id, path).await?;
                    loaded.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, DataFusionError>(derived)
                })
                .await?;
            if cached.fingerprint == want {
                self.touch(session_id, &slot, cached.memory_size());
                return Ok(Arc::clone(cached));
            }
            // Stale: drop this slot and retry. The replacement
            // `OnceCell` either parses fresh or, if another caller
            // raced ahead, reuses their parse.
            self.lock().remove(session_id);
        }
    }

    fn slot(&self, session_id: &str) -> Slot {
        let mut inner = self.lock();
        match inner.slots.get(session_id) {
            Some(s) => Arc::clone(s),
            None => {
                let slot: Slot = Arc::new(OnceCell::new());
                inner
                    .slots
                    .insert(session_id.to_string(), Arc::clone(&slot));
                slot
            }
        }
    }

    /// Mark `slot` as most recently used, counting its bytes on first
    /// fill, then evict least recently used sessions past the bound.
    /// A slot evicted or replaced while its parse ran is not counted:
    /// the map no longer holds it, so its bytes are the caller's.
    fn touch(&self, session_id: &str, slot: &Slot, bytes: usize) {
        let mut inner = self.lock();
        if !inner
            .slots
            .get(session_id)
            .is_some_and(|s| Arc::ptr_eq(s, slot))
        {
            return;
        }
        match inner.order.iter().position(|(id, _)| id == session_id) {
            Some(i) => {
                let entry = inner
                    .order
                    .remove(i)
                    .expect("position came from this deque");
                inner.order.push_back(entry);
            }
            None => {
                inner.order.push_back((session_id.to_string(), bytes));
                inner.resident += bytes;
            }
        }
        while inner.resident > self.bound && inner.order.len() > 1 {
            let (id, bytes) = inner.order.pop_front().expect("len > 1");
            inner.slots.remove(&id);
            inner.resident -= bytes;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("cache lock")
    }
}

impl Inner {
    fn remove(&mut self, session_id: &str) {
        self.slots.remove(session_id);
        if let Some(i) = self.order.iter().position(|(id, _)| id == session_id)
            && let Some((_, bytes)) = self.order.remove(i)
        {
            self.resident -= bytes;
        }
    }
}

impl DerivedSession {
    /// Arrow memory of the session's batches, the cache's unit of
    /// residency
    pub fn memory_size(&self) -> usize {
        self.batch.get_array_memory_size() + self.tool_uses.get_array_memory_size()
    }
}

async fn parse(id: String, path: PathBuf) -> Result<Arc<DerivedSession>> {
    let derived = tokio::task::spawn_blocking(move || derive_session(&id, &path))
        .await
        .map_err(|e| DataFusionError::Execution(format!("derive task failed: {e}")))?
        .map_err(|e| DataFusionError::Execution(format!("derive failed: {e}")))?;
    Ok(Arc::new(derived))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session file whose derived batches are roughly `lines` user
    /// entries of text
    fn write_session(dir: &Path, n: usize, lines: usize) -> PathBuf {
        let path = dir.join(format!("s{n}.jsonl"));
        let text: String = (0..lines)
            .map(|i| {
                format!(
                    "{{\"type\":\"user\",\"uuid\":\"u{i}\",\"timestamp\":\"2025-01-01T00:00:{:02}Z\",\
                     \"message\":{{\"role\":\"user\",\"content\":\"line {i} of session {n} {}\"}}}}\n",
                    i % 60,
                    "x".repeat(200)
                )
            })
            .collect();
        std::fs::write(&path, text).unwrap();
        path
    }

    #[tokio::test]
    async fn residency_stays_under_the_bound_across_a_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..12).map(|n| write_session(dir.path(), n, 20)).collect();
        let probe = SessionCache::with_bound(usize::MAX);
        let one = probe.get("s0", &paths[0]).await.unwrap().memory_size();
        assert!(one > 0);

        let cache = SessionCache::with_bound(one * 3);
        for (n, path) in paths.iter().enumerate() {
            cache.get(&format!("s{n}"), path).await.unwrap();
            assert!(
                cache.resident_bytes() <= one * 3,
                "after session {n}: {} resident over bound {}",
                cache.resident_bytes(),
                one * 3
            );
        }
        assert_eq!(cache.loaded(), 12);

        cache.get("s11", &paths[11]).await.unwrap();
        assert_eq!(cache.loaded(), 12, "the most recent session is still held");
        cache.get("s0", &paths[0]).await.unwrap();
        assert_eq!(cache.loaded(), 13, "the least recent session was evicted");
    }

    #[tokio::test]
    async fn a_session_larger_than_the_bound_is_held_alone() {
        let dir = tempfile::tempdir().unwrap();
        let small = write_session(dir.path(), 0, 2);
        let large = write_session(dir.path(), 1, 200);
        let cache = SessionCache::with_bound(1);
        cache.get("s0", &small).await.unwrap();
        let held = cache.get("s1", &large).await.unwrap();
        assert_eq!(cache.resident_bytes(), held.memory_size());
        assert_eq!(cache.lock().slots.len(), 1);
    }

    #[tokio::test]
    async fn a_rewritten_file_is_reparsed_and_recounted() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path(), 0, 5);
        let cache = SessionCache::with_bound(usize::MAX);
        let before = cache.get("s0", &path).await.unwrap().memory_size();
        assert_eq!(cache.resident_bytes(), before);

        let path = write_session(dir.path(), 0, 40);
        let after = cache.get("s0", &path).await.unwrap().memory_size();
        assert_eq!(cache.loaded(), 2);
        assert_eq!(cache.resident_bytes(), after);
        assert!(after > before);
    }
}
