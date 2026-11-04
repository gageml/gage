//! [`Store`]: an opened Gage store.
//!
//! A store is opened once per process. Opening verifies that a bare
//! repository is present and reads `gage.version` once, refusing a
//! store written by a newer Gage. Every operation is a method on the
//! handle; the type-specific interfaces (`NoteStore`, `DatasetStore`,
//! `SessionStore`) borrow it.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use gage_core::cache::{ensure_corpus_dir, store_corpus_dir, store_source_url};
use gage_core::config::default_cache_root;

use crate::StoreError;
use crate::admin::{STORE_VERSION, exists, migrate, refresh_hooks};
use crate::git::{CatFile, CatFileCell, git_cmd, git_in, run};
use crate::index::ObjectIndex;
use crate::sqlite_index::SqliteIndex;

/// The object index file inside a store's corpus directory under the
/// cache root
pub const OBJECTS_FILE: &str = "objects.db";

/// The object index file of the store at `store` under `cache_root`.
pub fn index_path(cache_root: &Path, store: &Path) -> PathBuf {
    store_corpus_dir(cache_root, store).join(OBJECTS_FILE)
}

/// Remove the object index file of the store at `store` under
/// `cache_root`. An absent file is the outcome the caller wants, so
/// it is not an error.
pub(crate) fn remove_index_file(cache_root: &Path, store: &Path) -> Result<(), StoreError> {
    let index = index_path(cache_root, store);
    match fs::remove_file(&index) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::Index(format!(
            "remove {}: {e}",
            index.display()
        ))),
    }
}

/// An opened Gage store: a bare Git repository whose `gage.version`
/// this build supports.
pub struct Store {
    path: PathBuf,
    /// The cache root the object index is kept under
    cache_root: PathBuf,
    version: u32,
    pub(crate) index: Box<dyn ObjectIndex>,
    /// The long-lived `cat-file` process every read goes through.
    pub(crate) cat_file: CatFileCell,
    /// Set by any write; decides whether `git gc --auto` runs at drop.
    pub(crate) wrote: Cell<bool>,
    /// Set by a delete or by a reconcile that dropped a ref; decides
    /// whether the index is pruned at drop.
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

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    /// [`StoreError::VersionMismatch`] when the version is newer than
    /// this build's, or older with no migration step from it. An older
    /// version with steps is migrated first. Hooks behind this build's
    /// are rewritten. Opens the object index in the store's corpus
    /// directory under the default cache root and reconciles it with
    /// the repository's refs.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Store::open_with_cache_root(path, &default_cache_root())
    }

    /// As [`Store::open`], with the object index kept under
    /// `cache_root`.
    pub fn open_with_cache_root(path: &Path, cache_root: &Path) -> Result<Store, StoreError> {
        if !exists(path) {
            return Err(StoreError::NotFound(path.to_path_buf()));
        }
        if let Some(format) = read_object_format(path)?
            && format != "sha1"
        {
            return Err(StoreError::UnsupportedObjectFormat(format));
        }
        let found = read_version(path)?;
        let version = if found < STORE_VERSION {
            migrate(path, found)?
        } else {
            found
        };
        if version != STORE_VERSION {
            return Err(StoreError::VersionMismatch {
                found: version,
                supported: STORE_VERSION,
            });
        }
        refresh_hooks(path)?;
        let corpus_dir = store_corpus_dir(cache_root, path);
        ensure_corpus_dir(&corpus_dir, &store_source_url(path))
            .map_err(|e| StoreError::Index(format!("create {}: {e}", corpus_dir.display())))?;
        let store = Store {
            path: path.to_path_buf(),
            cache_root: cache_root.to_path_buf(),
            version,
            index: Box::new(SqliteIndex::open(&index_path(cache_root, path))?),
            cat_file: RefCell::new(CatFile::spawn(path)?),
            wrote: Cell::new(false),
            deleted: Cell::new(false),
        };
        store.reconcile()?;
        Ok(store)
    }

    /// Open the store at `path` with a new object index: the index
    /// file is removed first, so the open's reconcile is a full walk
    /// of every ref.
    pub fn rebuild_index(path: &Path) -> Result<Store, StoreError> {
        Store::rebuild_index_with_cache_root(path, &default_cache_root())
    }

    /// As [`Store::rebuild_index`], with the object index kept under
    /// `cache_root`.
    pub fn rebuild_index_with_cache_root(
        path: &Path,
        cache_root: &Path,
    ) -> Result<Store, StoreError> {
        remove_index_file(cache_root, path)?;
        let store = Store::open_with_cache_root(path, cache_root)?;
        store.rebuild_key_refs()?;
        Ok(store)
    }

    /// A second handle on this store, opened at the same path with
    /// the same cache root. Its index is a separate connection to the
    /// same file.
    pub fn reopen(&self) -> Result<Store, StoreError> {
        Store::open_with_cache_root(&self.path, &self.cache_root)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The cache root the object index is kept under.
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// The object index file.
    pub fn index_path(&self) -> PathBuf {
        index_path(&self.cache_root, &self.path)
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
    let mut cmd: Command = git_cmd();
    cmd.args(["config", "--file"])
        .arg(&config)
        .args(["--get", "extensions.objectFormat"]);
    match run(cmd) {
        Ok(text) => Ok(Some(text.trim().to_string())),
        Err(StoreError::Git { status, .. }) if status.code() == Some(1) => Ok(None),
        Err(e) => Err(e),
    }
}

fn read_version(path: &Path) -> Result<u32, StoreError> {
    read_config_u32(path, "gage.version")?
        .ok_or_else(|| StoreError::VersionMissing(path.to_path_buf()))
}

/// The integer at `key` in the store's config, or `None` when the key
/// is absent.
pub(crate) fn read_config_u32(path: &Path, key: &str) -> Result<Option<u32>, StoreError> {
    let text = match run(git_in(path, ["config", "--get", key])) {
        Ok(text) => text,
        // `git config --get` exits 1 when the key is absent.
        Err(StoreError::Git { status, .. }) if status.code() == Some(1) => return Ok(None),
        Err(e) => return Err(e),
    };
    text.trim()
        .parse()
        .map(Some)
        .map_err(|e| StoreError::Parse(format!("{key} {text:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{HOOK_VERSION, UPDATE_HOOK, UPDATE_HOOK_PATH};
    use crate::test_support::{init_for_test, open};

    #[test]
    fn open_reads_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        let store = open(&path).unwrap();
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
        init_for_test(&path);
        // git runs when the sampled directory holds more than
        // gc.auto/256 rounded up, so two probes clear a threshold of
        // 1; in the foreground so the outcome is observable
        run(git_in(&path, ["config", "gc.auto", "1"])).unwrap();
        run(git_in(&path, ["config", "gc.autoDetach", "false"])).unwrap();
        let probes = blobs_under_17(&path, 2);
        {
            let store = open(&path).unwrap();
            crate::NoteStore::from(&store)
                .create(crate::NoteInput {
                    name: "n",
                    value: crate::NoteValue::Text("v".into()),
                    author: "user:test",
                    target: None,
                    metadata: None,
                    work_id: None,
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
            open(&tmp.path().join("nope.git")).unwrap_err(),
            StoreError::NotFound(_)
        ));
    }

    #[test]
    fn open_refuses_newer_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        let newer = (STORE_VERSION + 1).to_string();
        run(git_in(&path, ["config", "gage.version", &newer])).unwrap();
        assert!(matches!(
            open(&path).unwrap_err(),
            StoreError::VersionMismatch { found, supported }
                if found == STORE_VERSION + 1 && supported == STORE_VERSION
        ));
    }

    #[test]
    fn open_refuses_sha256_object_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        run(git_in(
            &path,
            ["config", "extensions.objectFormat", "sha256"],
        ))
        .unwrap();
        assert!(matches!(
            open(&path).unwrap_err(),
            StoreError::UnsupportedObjectFormat(f) if f == "sha256"
        ));
    }

    #[test]
    fn open_refuses_missing_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        run(git_in(&path, ["config", "--unset", "gage.version"])).unwrap();
        assert!(matches!(
            open(&path).unwrap_err(),
            StoreError::VersionMissing(_)
        ));
    }

    #[test]
    fn open_refuses_an_older_version_with_no_migration_step() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        run(git_in(&path, ["config", "gage.version", "0"])).unwrap();
        let err = open(&path).unwrap_err();
        assert!(
            matches!(err, StoreError::VersionMismatch { found: 0, supported } if supported == STORE_VERSION),
            "{err}"
        );
        assert!(
            err.to_string().contains("no migration is available"),
            "{err}"
        );
    }

    /// A store whose recorded hook version is behind this build's has
    /// both hooks rewritten at open; one at this build's version is
    /// left as it is.
    #[test]
    fn open_rewrites_hooks_behind_this_build() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        let hook = path.join(UPDATE_HOOK_PATH);
        let stale = "#!/bin/sh\nexit 0\n";

        fs::write(&hook, stale).unwrap();
        run(git_in(&path, ["config", "--unset", "gage.hookVersion"])).unwrap();
        drop(open(&path).unwrap());
        assert_eq!(fs::read_to_string(&hook).unwrap(), UPDATE_HOOK);
        assert_eq!(
            run(git_in(&path, ["config", "--get", "gage.hookVersion"]))
                .unwrap()
                .trim(),
            HOOK_VERSION.to_string()
        );

        fs::write(&hook, stale).unwrap();
        drop(open(&path).unwrap());
        assert_eq!(fs::read_to_string(&hook).unwrap(), stale);
    }
}
