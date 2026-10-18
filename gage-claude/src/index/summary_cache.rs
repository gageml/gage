//! On-disk per-session summary cache.
//!
//! One file per session at `<summary dir>/<id>`, holding the
//! rmp-serde-encoded `ClaudeSessionAttrs` produced during derivation.
//! The directory also holds the manifest and the reconcile lock, which
//! the session listing skips. Reconcile and the session-table lazy
//! path write; `IndexStore::session_attrs` reads.
//!
//! Validation is by mtime: a cache file whose mtime is at least the
//! JSONL's mtime is treated as fresh. JSONL files only grow (Claude
//! Code appends), so any write moves mtime forward and invalidates the
//! cache. Worst case is a few messages short during a reconcile-vs-write
//! race; the lag self-heals on the next JSONL write.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::derive::ClaudeSessionAttrs;

/// Cache format version, recorded in the manifest. Bumped on any
/// schema or semantic change to `ClaudeSessionAttrs` (new field,
/// changed title heuristic, fixed token accounting, ...). A directory
/// whose manifest records another version is discarded and rebuilt.
pub const VERSION: u32 = 7;

/// The manifest file beside the session files
pub(crate) const MANIFEST_FILE: &str = "manifest.json";

/// The reconcile lock file beside the session files
pub(crate) const LOCK_FILE: &str = "reconcile.lock";

/// Suffix of a file being written; renamed into place when complete
const TMP_SUFFIX: &str = ".tmp";

fn cache_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(id)
}

/// Whether a directory entry name is a session file rather than the
/// manifest, the lock, or a write in progress.
fn is_session_file(name: &str) -> bool {
    name != MANIFEST_FILE && name != LOCK_FILE && !name.ends_with(TMP_SUFFIX)
}

/// Read the cached summary for `id`. Returns `None` on missing file,
/// stale cache (cache.mtime < jsonl_mtime), or decode failure.
pub(crate) fn read(dir: &Path, id: &str, jsonl_mtime: SystemTime) -> Option<ClaudeSessionAttrs> {
    let path = cache_path(dir, id);
    let cache_mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
    if cache_mtime < jsonl_mtime {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    rmp_serde::from_slice(&bytes).ok()
}

/// Write the summary for `id`. Atomic via tmp file + rename.
pub(crate) fn write(dir: &Path, id: &str, attrs: &ClaudeSessionAttrs) -> std::io::Result<()> {
    let bytes = rmp_serde::to_vec(attrs).map_err(std::io::Error::other)?;
    let final_path = cache_path(dir, id);
    let tmp_path = dir.join(format!("{id}{TMP_SUFFIX}"));
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Remove the cached summary for `id`. Silent on missing file.
pub(crate) fn remove(dir: &Path, id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(cache_path(dir, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove every session file, leaving the manifest and the lock.
/// Called by `rebuild`.
pub(crate) fn remove_all(dir: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if is_session_file(&entry.file_name().to_string_lossy()) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Session ids with a present cache file. One readdir; lets reconcile
/// flag missing-cache as dirty without statting every session.
pub(crate) fn existing_ids(dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_session_file(&name) {
            out.insert(name);
        }
    }
    out
}
