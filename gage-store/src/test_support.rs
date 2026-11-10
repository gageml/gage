//! Helpers for the crate's tests.
//!
//! Every store a test builds is verified with `git fsck --strict` when
//! the test ends, so the whole suite doubles as a corpus check on the
//! objects the writer produces.

use std::path::{Path, PathBuf};

use crate::git::{git_in, run};
use crate::object::object_ref;
use crate::writer::{TreeInput, commit_tree, mktree, write_blob};
use crate::{Store, StoreError, init_with_cache_root};

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
    let store = open(&path).unwrap();
    (store, FsckGuard { path })
}

/// The cache root a test keeps beside the store at `store`: `cache`
/// in the store's directory, so nothing a test opens reaches the
/// default cache root.
pub(crate) fn cache_root_for(store: &Path) -> PathBuf {
    store.with_file_name("cache")
}

/// Open the store at `path` with its index under [`cache_root_for`].
pub(crate) fn open(path: &Path) -> Result<Store, StoreError> {
    Store::open_with_cache_root(path, &cache_root_for(path))
}

/// `init` with the index under [`cache_root_for`] and git's automatic
/// gc switched off, so no background git runs against a temporary
/// directory the test is about to remove.
pub(crate) fn init_for_test(path: &Path) {
    init_with_cache_root(path, &cache_root_for(path)).unwrap();
    run(git_in(path, ["config", "gc.auto", "0"])).unwrap();
}

/// An fsck guard for a store the test opens itself.
pub(crate) fn fsck_guard(path: &Path) -> FsckGuard {
    FsckGuard {
        path: path.to_path_buf(),
    }
}

/// Rewrite the object `id` so its tip carries `name` as a blob with
/// `content`, as a parentless commit written around the writer: the
/// way a newer build's object, or a damaged one, arrives. The index
/// sees the move at the next open; see [`reopen`].
pub(crate) fn plant_blob(store: &Store, id: &str, name: &str, content: &str) -> String {
    let tip = store.rev_parse(&object_ref(id)).unwrap().unwrap();
    let mut entries: Vec<(String, String, String)> = store
        .list_tree(&tip)
        .unwrap()
        .into_iter()
        .filter(|e| e.name != name)
        .map(|e| (e.mode, e.sha, e.name))
        .collect();
    let blob = write_blob(store.path(), content.as_bytes()).unwrap();
    entries.push(("100644".to_string(), blob, name.to_string()));
    let inputs: Vec<TreeInput<'_>> = entries
        .iter()
        .map(|(mode, sha, name)| TreeInput { mode, sha, name })
        .collect();
    let tree = mktree(store.path(), &inputs).unwrap();
    let commit = commit_tree(store.path(), &tree, "planted", &[]).unwrap();
    run(git_in(
        store.path(),
        ["update-ref", &object_ref(id), &commit],
    ))
    .unwrap();
    commit
}

/// Close `store` and open it again, so the index reconciles with refs
/// moved around the writer.
pub(crate) fn reopen(store: Store) -> Store {
    let path = store.path().to_path_buf();
    drop(store);
    open(&path).unwrap()
}

/// The `(loose, packed)` object counts of the object database at
/// `path`.
pub(crate) fn object_counts(path: &Path) -> (u64, u64) {
    let out = run(git_in(path, ["count-objects", "-v"])).unwrap();
    let field = |name: &str| {
        out.lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    (field("count:"), field("in-pack:"))
}
