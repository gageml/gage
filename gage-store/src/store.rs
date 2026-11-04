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
use crate::admin::{STORE_VERSION, exists, migrate, ref_generations, refresh_hooks};
use crate::git::{CatFile, CatFileCell, git_cmd, git_in, run};
use crate::index::ObjectIndex;
use crate::sqlite_index::SqliteIndex;

/// The object index file inside a store's corpus directory under the
/// cache root
pub const OBJECTS_FILE: &str = "objects.db";

/// The git directory a store path names: the path itself for a bare
/// repository, or the repository behind `path/.git` for a working
/// tree, with a gitfile followed to a worktree's or submodule's
/// directory. A path that is neither is returned as given; nothing
/// above it is searched.
pub fn locate(path: &Path) -> PathBuf {
    if path.join("HEAD").is_file() || !path.join(".git").exists() {
        return path.to_path_buf();
    }
    match run(git_in(path, ["rev-parse", "--absolute-git-dir"])) {
        Ok(dir) => PathBuf::from(dir.trim_end()),
        Err(e) => {
            tracing::debug!(path = %path.display(), "git dir not resolved: {e}");
            path.to_path_buf()
        }
    }
}

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
    /// Whether the repository carries Gage's `gage.version` stamp. A
    /// stamped repository is one Gage created and configures; an
    /// unstamped one is a guest, a repository of someone else's that
    /// Gage reads and writes under `refs/gage/` only.
    owned: bool,
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
            .field("owned", &self.owned)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open the store at `path`, located as [`locate`] does. Fails with
    /// [`StoreError::NotFound`] when no repository is there and
    /// [`StoreError::VersionMismatch`] when the version is newer than
    /// this build's, or older with no migration step from it. A
    /// repository stamped with `gage.version` is owned: an older
    /// version with steps is migrated first, and hooks behind this
    /// build's are rewritten. An unstamped repository is a guest: its
    /// version is the generation of the `refs/gage/` namespace it
    /// holds, this build's when the namespace is empty, and nothing
    /// outside that namespace is written. Opens the object index in
    /// the store's corpus directory under the default cache root and
    /// reconciles it with the repository's refs.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Store::open_with_cache_root(path, &default_cache_root())
    }

    /// As [`Store::open`], with the object index kept under
    /// `cache_root`.
    pub fn open_with_cache_root(path: &Path, cache_root: &Path) -> Result<Store, StoreError> {
        let path = &locate(path);
        if !exists(path) {
            return Err(StoreError::NotFound(path.to_path_buf()));
        }
        if let Some(format) = read_object_format(path)?
            && format != "sha1"
        {
            return Err(StoreError::UnsupportedObjectFormat(format));
        }
        let (version, owned) = match read_config_u32(path, "gage.version")? {
            Some(found) => {
                let version = if found < STORE_VERSION {
                    migrate(path, found)?
                } else {
                    found
                };
                (version, true)
            }
            None => (guest_version(path)?, false),
        };
        if version != STORE_VERSION {
            return Err(StoreError::VersionMismatch {
                found: version,
                supported: STORE_VERSION,
            });
        }
        if owned {
            refresh_hooks(path)?;
        }
        let corpus_dir = store_corpus_dir(cache_root, path);
        ensure_corpus_dir(&corpus_dir, &store_source_url(path))
            .map_err(|e| StoreError::Index(format!("create {}: {e}", corpus_dir.display())))?;
        let store = Store {
            path: path.to_path_buf(),
            cache_root: cache_root.to_path_buf(),
            version,
            owned,
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

    /// The store's `gage.version`, or the generation of a guest's
    /// `refs/gage/` namespace.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether the repository is one Gage created and configures. A
    /// guest store is read and written under `refs/gage/` only, with
    /// no config, hooks, or migration.
    pub fn is_owned(&self) -> bool {
        self.owned
    }
}

/// The generation of an unstamped repository: the one its `refs/gage/`
/// namespace holds, or this build's when it holds none. A namespace
/// at another generation is reported as that generation, which the
/// caller refuses, since a guest is never migrated.
fn guest_version(path: &Path) -> Result<u32, StoreError> {
    let generations = ref_generations(path)?;
    if generations.contains(&STORE_VERSION) || generations.is_empty() {
        return Ok(STORE_VERSION);
    }
    Ok(*generations
        .iter()
        .next()
        .expect("a non-empty set has a first element"))
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
    fn open_of_an_unstamped_store_is_a_guest_at_this_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init_for_test(&path);
        run(git_in(&path, ["config", "--unset", "gage.version"])).unwrap();
        let store = open(&path).unwrap();
        assert!(!store.is_owned());
        assert_eq!(store.version(), STORE_VERSION);
    }

    /// A working tree repository with a commit on a branch, as a
    /// project repository is.
    fn working_tree(dir: &Path) -> PathBuf {
        let wt = dir.join("project");
        fs::create_dir_all(&wt).unwrap();
        run(git_in(&wt, ["init", "--quiet", "--initial-branch=main"])).unwrap();
        run(git_in(
            &wt,
            [
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "x",
            ],
        ))
        .unwrap();
        wt
    }

    fn local_config(path: &Path) -> String {
        run(git_in(path, ["config", "--list", "--local"])).unwrap()
    }

    fn ref_names(path: &Path) -> Vec<String> {
        run(git_in(path, ["for-each-ref", "--format=%(refname)"]))
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn locate_maps_a_path_to_its_git_directory_without_searching_upward() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("store.git");
        init_for_test(&bare);
        assert_eq!(locate(&bare), bare);

        let wt = working_tree(tmp.path());
        assert_eq!(locate(&wt), wt.join(".git"));

        let checkout = tmp.path().join("checkout");
        run(git_in(
            &wt,
            ["worktree", "add", "--quiet", checkout.to_str().unwrap()],
        ))
        .unwrap();
        assert_eq!(
            locate(&checkout),
            wt.join(".git").join("worktrees").join("checkout")
        );

        let inside = wt.join("src");
        fs::create_dir_all(&inside).unwrap();
        assert_eq!(locate(&inside), inside);
        let missing = tmp.path().join("nope");
        assert_eq!(locate(&missing), missing);
    }

    /// A guest open installs nothing: no stamp, no hooks, no config.
    /// Its writes land under `refs/gage/` and nowhere else.
    #[test]
    fn a_guest_is_read_and_written_under_its_namespace_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = working_tree(tmp.path());
        let git_dir = wt.join(".git");
        let config_before = local_config(&git_dir);
        let refs_before = ref_names(&git_dir);

        let store = Store::open_with_cache_root(&wt, &tmp.path().join("cache")).unwrap();
        assert!(!store.is_owned());
        assert_eq!(store.path(), git_dir);
        assert_eq!(store.version(), STORE_VERSION);
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
        drop(store);

        assert_eq!(local_config(&git_dir), config_before);
        assert!(!git_dir.join(UPDATE_HOOK_PATH).exists());
        assert!(!git_dir.join(crate::admin::PRE_PUSH_HOOK_PATH).exists());
        let refs_after = ref_names(&git_dir);
        let added: Vec<&String> = refs_after
            .iter()
            .filter(|r| !refs_before.contains(r))
            .collect();
        assert_eq!(added.len(), 1, "{added:?}");
        assert!(added[0].starts_with(crate::refs::ROOT), "{}", added[0]);

        let reopened = Store::open_with_cache_root(&wt, &tmp.path().join("cache")).unwrap();
        assert_eq!(crate::NoteStore::from(&reopened).iter().unwrap().count(), 1);
    }

    #[test]
    fn a_guest_at_another_generation_is_refused_without_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = working_tree(tmp.path());
        let git_dir = wt.join(".git");
        let head = run(git_in(&git_dir, ["rev-parse", "HEAD"])).unwrap();
        run(git_in(
            &git_dir,
            ["update-ref", "refs/gage/0/object/x", head.trim()],
        ))
        .unwrap();
        assert!(matches!(
            open(&wt).unwrap_err(),
            StoreError::VersionMismatch { found: 0, .. }
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
