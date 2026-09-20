//! [`Store`]: an opened Gage store.
//!
//! A store is opened once per process. Opening verifies that a bare
//! repository is present and reads `gage.version` once, refusing a
//! store written by a newer Gage. Every operation is a method on the
//! handle; the type-specific interfaces (`NoteStore`, `DatasetStore`,
//! `SessionStore`) borrow it.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use crate::StoreError;
use crate::admin::{STORE_VERSION, exists};
use crate::git::{CatFile, CatFileCell, git_in, run};
use crate::index::ObjectIndex;
use crate::sqlite_index::SqliteIndex;

/// Index file, relative to the store's parent directory (Gage home).
pub(crate) const INDEX_FILE: &str = "cache/object-index.sqlite";

/// An opened Gage store: a bare Git repository whose `gage.version`
/// this build supports.
pub struct Store {
    path: PathBuf,
    version: u32,
    pub(crate) index: Box<dyn ObjectIndex>,
    /// The long-lived `cat-file` process every read goes through.
    pub(crate) cat_file: CatFileCell,
    /// Set by any write; decides whether `git gc --auto` runs at drop.
    pub(crate) wrote: Cell<bool>,
    /// Set by a delete; decides whether the index is pruned at drop.
    pub(crate) deleted: Cell<bool>,
}

impl Drop for Store {
    /// End-of-command housekeeping, as git runs `gc --auto` at the end
    /// of a porcelain command. A delete leaves index rows for versions
    /// nothing reaches, pruned once here however many deletes ran. Any
    /// write hands git the decision on collecting: `gc --auto` returns
    /// at once when its thresholds are not met and otherwise detaches
    /// into the background with the default prune expiry, which is
    /// what keeps a concurrent writer safe. Neither outcome can be
    /// returned from a drop, so failures are logged.
    fn drop(&mut self) {
        if self.deleted.get()
            && let Err(e) = self.in_index_transaction(|| self.index.prune())
        {
            tracing::warn!("index prune at close: {e}");
        }
        if self.wrote.get()
            && let Err(e) = run(git_in(&self.path, ["gc", "--auto", "--quiet"]))
        {
            tracing::warn!("git gc --auto at close: {e}");
        }
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open the store at `path`. Fails with [`StoreError::NotFound`]
    /// when no repository is there, [`StoreError::VersionMissing`] when
    /// the repository carries no `gage.version`, and
    /// [`StoreError::VersionMismatch`] when the version is not the one
    /// this build supports. Opens the object index beside the store
    /// (`cache/object-index.sqlite` under the store's parent) and
    /// reconciles it with the repository's refs.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if !exists(path) {
            return Err(StoreError::NotFound(path.to_path_buf()));
        }
        if let Some(format) = read_object_format(path)?
            && format != "sha1"
        {
            return Err(StoreError::UnsupportedObjectFormat(format));
        }
        let version = read_version(path)?;
        if version != STORE_VERSION {
            return Err(StoreError::VersionMismatch {
                found: version,
                supported: STORE_VERSION,
            });
        }
        let index_path = path
            .parent()
            .map(|p| p.join(INDEX_FILE))
            .unwrap_or_else(|| PathBuf::from(INDEX_FILE));
        let store = Store {
            path: path.to_path_buf(),
            version,
            index: Box::new(SqliteIndex::open(&index_path)?),
            cat_file: RefCell::new(CatFile::spawn(path)?),
            wrote: Cell::new(false),
            deleted: Cell::new(false),
        };
        store.reconcile()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The store's `gage.version`.
    pub fn version(&self) -> u32 {
        self.version
    }
}

/// `extensions.objectFormat`, or `None` when unset (SHA-1). The writer
/// produces SHA-1 objects only.
fn read_object_format(path: &Path) -> Result<Option<String>, StoreError> {
    // Read the file directly: with the extension set on a SHA-1
    // repository git refuses to open it at all, and the answer here
    // has to be the format, not that refusal.
    let config = path.join("config");
    let config = config.to_string_lossy();
    match run(git_in(
        path,
        [
            "config",
            "--file",
            &config,
            "--get",
            "extensions.objectFormat",
        ],
    )) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(StoreError::Git { status, .. }) if status.code() == Some(1) => Ok(None),
        Err(e) => Err(e),
    }
}

fn read_version(path: &Path) -> Result<u32, StoreError> {
    let text = match run(git_in(path, ["config", "--get", "gage.version"])) {
        Ok(text) => text,
        // `git config --get` exits 1 when the key is absent.
        Err(StoreError::Git { status, .. }) if status.code() == Some(1) => {
            return Err(StoreError::VersionMissing(path.to_path_buf()));
        }
        Err(e) => return Err(e),
    };
    text.trim()
        .parse()
        .map_err(|e| StoreError::Parse(format!("gage.version {text:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init;

    #[test]
    fn open_reads_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.path(), path);
        assert_eq!(store.version(), STORE_VERSION);
    }

    /// Loose blobs whose SHAs start with `17`, the directory git
    /// samples to estimate the loose object count for `gc --auto`.
    fn blobs_under_17(path: &Path, count: usize) -> Vec<String> {
        (0u32..)
            .map(|n| crate::writer::write_blob(path, format!("probe {n}").as_bytes()).unwrap())
            .filter(|sha| sha.starts_with("17"))
            .take(count)
            .collect()
    }

    #[test]
    fn writes_hand_git_gc_auto_the_decision_at_close() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init(&path).unwrap();
        // git runs when the sampled directory holds more than
        // gc.auto/256 rounded up, so two probes clear a threshold of
        // 1; in the foreground so the outcome is observable
        run(git_in(&path, ["config", "gc.auto", "1"])).unwrap();
        run(git_in(&path, ["config", "gc.autoDetach", "false"])).unwrap();
        let probes = blobs_under_17(&path, 2);
        {
            let store = Store::open(&path).unwrap();
            crate::NoteStore::from(&store)
                .create(crate::NoteInput {
                    name: "n",
                    value: "v",
                    author: "user:test",
                    targets: &[],
                })
                .unwrap();
            assert!(store.wrote.get());
        }
        let counts = run(git_in(&path, ["count-objects", "-v"])).unwrap();
        assert!(counts.contains("packs: 1"), "{counts}");
        // The unreferenced probes are younger than the prune expiry
        for probe in &probes {
            assert!(
                path.join("objects")
                    .join(&probe[..2])
                    .join(&probe[2..])
                    .exists()
            );
        }
    }

    #[test]
    fn open_of_missing_store_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            Store::open(&tmp.path().join("nope.git")).unwrap_err(),
            StoreError::NotFound(_)
        ));
    }

    #[test]
    fn open_refuses_newer_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init(&path).unwrap();
        let newer = (STORE_VERSION + 1).to_string();
        run(git_in(&path, ["config", "gage.version", &newer])).unwrap();
        assert!(matches!(
            Store::open(&path).unwrap_err(),
            StoreError::VersionMismatch { found, supported }
                if found == STORE_VERSION + 1 && supported == STORE_VERSION
        ));
    }

    #[test]
    fn open_refuses_sha256_object_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init(&path).unwrap();
        run(git_in(
            &path,
            ["config", "extensions.objectFormat", "sha256"],
        ))
        .unwrap();
        assert!(matches!(
            Store::open(&path).unwrap_err(),
            StoreError::UnsupportedObjectFormat(f) if f == "sha256"
        ));
    }

    #[test]
    fn open_refuses_missing_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init(&path).unwrap();
        run(git_in(&path, ["config", "--unset", "gage.version"])).unwrap();
        assert!(matches!(
            Store::open(&path).unwrap_err(),
            StoreError::VersionMissing(_)
        ));
    }
}
