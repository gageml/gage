//! On-disk per-session summary cache.
//!
//! One file per session at `<cache>/session/v{N}/<id>`, holding the
//! rmp-serde-encoded `SessionSummary` produced during derivation.
//! Reconcile writes; `IndexStore::session_summary` reads.
//!
//! Validation is by mtime: a cache file whose mtime is at least the
//! JSONL's mtime is treated as fresh. JSONL files only grow (Claude
//! Code appends), so any write moves mtime forward and invalidates the
//! cache. Worst case is a few messages short during a reconcile-vs-write
//! race; the lag self-heals on the next JSONL write.
//!
//! The `v{N}` path component is bumped on any schema or semantic change
//! to `SessionSummary` derivation; stale `v{N}` directories are removed
//! by reconcile.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::derive::SessionSummary;

/// Cache format version. Bumped on any schema or semantic change to
/// `SessionSummary` (new field, changed title heuristic, fixed token
/// accounting, ...). Stale `v{N}` directories are cleaned up by
/// reconcile.
pub const VERSION: u32 = 1;

fn versioned_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("session").join(format!("v{VERSION}"))
}

fn cache_path(cache_dir: &Path, id: &str) -> PathBuf {
    versioned_dir(cache_dir).join(id)
}

/// Read the cached summary for `id`. Returns `None` on missing file,
/// stale cache (cache.mtime < jsonl_mtime), or decode failure.
pub(crate) fn read(cache_dir: &Path, id: &str, jsonl_mtime: SystemTime) -> Option<SessionSummary> {
    let path = cache_path(cache_dir, id);
    let cache_mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
    if cache_mtime < jsonl_mtime {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    rmp_serde::from_slice(&bytes).ok()
}

/// Write the summary for `id`. Atomic via tmp file + rename. Creates
/// the versioned directory on first call.
pub(crate) fn write(cache_dir: &Path, id: &str, summary: &SessionSummary) -> std::io::Result<()> {
    let dir = versioned_dir(cache_dir);
    std::fs::create_dir_all(&dir)?;
    let bytes = rmp_serde::to_vec(summary).map_err(std::io::Error::other)?;
    let final_path = cache_path(cache_dir, id);
    let tmp_path = dir.join(format!("{id}.tmp"));
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Remove the cached summary for `id`. Silent on missing file.
pub(crate) fn remove(cache_dir: &Path, id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(cache_path(cache_dir, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove the entire summary cache tree, all versions. Called by
/// `rebuild`.
pub(crate) fn remove_all(cache_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(cache_dir.join("session")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove `session/v{N}` directories whose `N` is not the current
/// `VERSION`. Mirrors the text-index version cleanup.
pub(crate) fn remove_stale_versions(cache_dir: &Path) {
    let parent = cache_dir.join("session");
    let current = format!("v{VERSION}");
    let entries = match std::fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && entry.file_name().to_string_lossy() != current {
            tracing::info!(path = %path.display(), "removing stale session-cache version");
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), "failed to remove: {e}");
            }
        }
    }
}

/// Session ids with a present cache file in the current version
/// directory. One readdir; lets reconcile flag missing-cache as dirty
/// without statting every session.
pub(crate) fn existing_ids(cache_dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let entries = match std::fs::read_dir(versioned_dir(cache_dir)) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".tmp") {
            continue;
        }
        out.insert(name);
    }
    out
}
