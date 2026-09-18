//! [`Store`]: an opened Gage store.
//!
//! A store is opened once per process. Opening verifies that a bare
//! repository is present and reads `gage.version` once, refusing a
//! store written by a newer Gage. Every operation is a method on the
//! handle; the type-specific interfaces (`NoteStore`, `DatasetStore`,
//! `SessionStore`) borrow it.

use std::path::{Path, PathBuf};

use crate::StoreError;
use crate::admin::{STORE_VERSION, exists};
use crate::git::{git_in, run};
use crate::index::ObjectIndex;
use crate::sqlite_index::SqliteIndex;

/// Index file, relative to the store's parent directory (Gage home).
const INDEX_FILE: &str = "cache/object-index.sqlite";

/// An opened Gage store: a bare Git repository whose `gage.version`
/// this build supports.
pub struct Store {
    path: PathBuf,
    version: u32,
    pub(crate) index: Box<dyn ObjectIndex>,
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
