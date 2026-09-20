//! Helpers for the crate's tests.
//!
//! Every store a test builds is verified with `git fsck --strict` when
//! the test ends, so the whole suite doubles as a corpus check on the
//! objects the writer produces.

use std::path::{Path, PathBuf};

use crate::git::{git_in, run};
use crate::{Store, init};

/// Runs `git fsck --strict` on the store at `path` when dropped and
/// panics on failure. Skipped while unwinding so a failing test
/// reports its own assertion rather than a second panic.
pub(crate) struct FsckGuard {
    path: PathBuf,
}

impl Drop for FsckGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if let Err(e) = run(git_in(&self.path, ["fsck", "--strict", "--no-progress"])) {
            panic!("fsck of {} failed: {e}", self.path.display());
        }
    }
}

/// Create and open a store under `dir`, guarded by fsck at drop. Bind
/// the guard so it outlives the test body: `let (store, _fsck) = ...`.
pub(crate) fn open_store(dir: &Path) -> (Store, FsckGuard) {
    let path = dir.join("store.git");
    init_for_test(&path);
    let store = Store::open(&path).unwrap();
    (store, FsckGuard { path })
}

/// `init` with git's automatic gc switched off, so no background git
/// runs against a temporary directory the test is about to remove.
pub(crate) fn init_for_test(path: &Path) {
    init(path).unwrap();
    run(git_in(path, ["config", "gc.auto", "0"])).unwrap();
}

/// An fsck guard for a store the test opens itself.
pub(crate) fn fsck_guard(path: &Path) -> FsckGuard {
    FsckGuard {
        path: path.to_path_buf(),
    }
}
