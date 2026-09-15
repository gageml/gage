//! Git backed Gage store.
//!
//! Every store operation runs the `git` binary found on `PATH`. No Git
//! library is linked: the store must interoperate with other Git
//! repositories (clone, push, pull), and the binary is the only complete
//! implementation of that surface.

use std::fmt;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use gage_core::config::gage_home;

mod dataset;
mod note;
mod writer;

pub use dataset::{
    DatasetRecord, DatasetSessionSummary, SessionAddOutcome, SessionOutcome, SessionSpec,
    dataset_add, dataset_add_at, dataset_list, dataset_list_at, dataset_resolve_id,
    dataset_resolve_id_at, dataset_sessions_add, dataset_sessions_add_at, dataset_sessions_list,
    dataset_sessions_list_at,
};
pub use note::{
    NoteFull, NoteInput, NoteRecord, note_add, note_add_at, note_delete, note_delete_at, note_edit,
    note_edit_at, note_get, note_get_at, note_list, note_list_at,
};

/// One entry from `git ls-tree -l`.
#[derive(Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: EntryKind,
    pub sha: String,
    /// Bytes on disk for a blob; `None` for a tree.
    pub size: Option<u64>,
    pub name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EntryKind {
    Blob,
    Tree,
    Commit,
}

impl EntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryKind::Blob => "blob",
            EntryKind::Tree => "tree",
            EntryKind::Commit => "commit",
        }
    }
}

/// List the entries directly under `reference`, resolved by
/// `git ls-tree -l`. `reference` is a git tree-ish (ref path, sha, or
/// `<ref>:<path>`).
pub fn ls(reference: &str) -> Result<Vec<TreeEntry>, StoreError> {
    ls_at(&store_path(), reference)
}

pub fn ls_at(path: &Path, reference: &str) -> Result<Vec<TreeEntry>, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let out = run(git_in(path, ["ls-tree", "-r", "-t", "-l", reference]))?;
    let mut entries = Vec::new();
    for line in out.lines() {
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let collected: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind_str, sha, size_str]: [&str; 4] =
            collected.try_into().map_err(|got: Vec<&str>| {
                StoreError::Parse(format!(
                    "ls-tree meta {meta:?}: expected 4 fields, got {}",
                    got.len()
                ))
            })?;
        let kind = match kind_str {
            "blob" => EntryKind::Blob,
            "tree" => EntryKind::Tree,
            "commit" => EntryKind::Commit,
            other => return Err(StoreError::Parse(format!("ls-tree type: {other}"))),
        };
        let size = match size_str {
            "-" => None,
            s => Some(
                s.parse::<u64>()
                    .map_err(|e| StoreError::Parse(format!("ls-tree size {s:?}: {e}")))?,
            ),
        };
        entries.push(TreeEntry {
            mode: mode.to_string(),
            kind,
            sha: sha.to_string(),
            size,
            name: name.to_string(),
        });
    }
    Ok(entries)
}

/// Dump the pretty-printed content of `reference` to the process's
/// stdout by running `git cat-file -p <reference>`.
pub fn cat(reference: &str) -> Result<(), StoreError> {
    cat_at(&store_path(), reference)
}

pub fn cat_at(path: &Path, reference: &str) -> Result<(), StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let mut cmd = git_in(path, ["cat-file", "-p", reference]);
    let status = cmd.status().map_err(StoreError::Spawn)?;
    if !status.success() {
        return Err(StoreError::Git {
            status,
            stderr: String::new(),
        });
    }
    Ok(())
}

/// Path to the store: `<gage_home>/store.git`.
pub fn store_path() -> PathBuf {
    gage_home().join("store.git")
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitOutcome {
    Created,
    Reinitialized,
}

/// Creates the store at [`store_path`], or reinitializes it if present.
pub fn init() -> Result<InitOutcome, StoreError> {
    init_at(&store_path())
}

/// Creates a bare repository at `path`, or reinitializes it if present.
///
/// Leading directories are created as needed. The empty template keeps
/// sample hooks and `description` out of the store and ignores any
/// `init.templateDir` in the user's Git config. The initial branch is
/// fixed so `HEAD` does not depend on `init.defaultBranch`.
pub fn init_at(path: &Path) -> Result<InitOutcome, StoreError> {
    let existing = exists(path);
    let mut cmd = git();
    cmd.args([
        "init",
        "--bare",
        "--quiet",
        "--template=",
        "--initial-branch=main",
    ])
    .arg(path);
    run(cmd)?;
    for (key, value) in STORE_CONFIG {
        run(git_in(path, ["config", key, value]))?;
    }
    Ok(if existing {
        InitOutcome::Reinitialized
    } else {
        InitOutcome::Created
    })
}

/// Settings every store carries. Refs only advance under push, and every
/// object received over the wire is verified. See store-init.md.
const STORE_CONFIG: [(&str, &str); 3] = [
    ("receive.denyDeletes", "true"),
    ("receive.denyNonFastForwards", "true"),
    ("transfer.fsckObjects", "true"),
];

/// What Git reports about the store: object and pack counts, disk size,
/// ref count, and configured remotes.
#[derive(Debug, PartialEq, Eq)]
pub struct StoreStatus {
    pub path: PathBuf,
    pub loose_objects: u64,
    pub packed_objects: u64,
    pub packs: u64,
    /// Bytes on disk for loose objects and packs together
    pub size: u64,
    pub refs: u64,
    /// Count of refs under `refs/gage/notes/`.
    pub note_refs: u64,
    pub remotes: Vec<Remote>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// Reads the status of the store at [`store_path`].
pub fn status() -> Result<StoreStatus, StoreError> {
    status_at(&store_path())
}

/// Before-and-after counts for a `gc` run.
#[derive(Debug)]
pub struct GcOutcome {
    pub before: StoreStatus,
    pub after: StoreStatus,
}

/// Run `git fsck --full` on the default store. Output is inherited to
/// the caller's stdout/stderr.
pub fn fsck() -> Result<(), StoreError> {
    fsck_at(&store_path())
}

pub fn fsck_at(path: &Path) -> Result<(), StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let mut cmd = git_in(path, ["fsck", "--full"]);
    let status = cmd.status().map_err(StoreError::Spawn)?;
    if !status.success() {
        return Err(StoreError::Git {
            status,
            stderr: String::new(),
        });
    }
    Ok(())
}

/// Run `git gc` on the default store, optionally with `--prune=<expire>`.
///
/// `git gc`'s output is passed through to the caller's stdout/stderr so
/// progress is visible.
pub fn gc(prune: Option<&str>) -> Result<GcOutcome, StoreError> {
    gc_at(&store_path(), prune)
}

/// Run `git gc` on the store at `path`.
pub fn gc_at(path: &Path, prune: Option<&str>) -> Result<GcOutcome, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let before = status_at(path)?;
    let mut cmd = git_in(path, ["gc"]);
    let prune_flag: String;
    if let Some(expire) = prune {
        prune_flag = format!("--prune={expire}");
        cmd.arg(&prune_flag);
    }
    let status = cmd.status().map_err(StoreError::Spawn)?;
    if !status.success() {
        return Err(StoreError::Git {
            status,
            stderr: String::new(),
        });
    }
    let after = status_at(path)?;
    Ok(GcOutcome { before, after })
}

/// Reads the status of the store at `path`. Fails with
/// [`StoreError::NotFound`] when no repository is there.
pub fn status_at(path: &Path) -> Result<StoreStatus, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let counts = parse_count_objects(&run(git_in(path, ["count-objects", "-v"]))?)?;
    let refs = run(git_in(path, ["for-each-ref", "--format=%(refname)"]))?
        .lines()
        .count() as u64;
    let note_refs = run(git_in(
        path,
        ["for-each-ref", "--format=%(refname)", "refs/gage/notes/"],
    ))?
    .lines()
    .count() as u64;
    let remotes = parse_remotes(&run(git_in(path, ["remote", "-v"]))?);
    Ok(StoreStatus {
        path: path.to_path_buf(),
        loose_objects: counts.count,
        packed_objects: counts.in_pack,
        packs: counts.packs,
        size: (counts.size + counts.size_pack) * 1024,
        refs,
        note_refs,
        remotes,
    })
}

pub(crate) fn exists(path: &Path) -> bool {
    path.join("HEAD").is_file()
}

/// The `git count-objects -v` fields this crate reads. Sizes are in KiB
/// as git reports them.
#[derive(Debug, Default, PartialEq, Eq)]
struct CountObjects {
    count: u64,
    size: u64,
    in_pack: u64,
    packs: u64,
    size_pack: u64,
}

fn parse_count_objects(output: &str) -> Result<CountObjects, StoreError> {
    let mut counts = CountObjects::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            return Err(StoreError::Parse(format!("count-objects line: {line}")));
        };
        let value: u64 = value
            .trim()
            .parse()
            .map_err(|e| StoreError::Parse(format!("count-objects value: {line}: {e}")))?;
        match key {
            "count" => counts.count = value,
            "size" => counts.size = value,
            "in-pack" => counts.in_pack = value,
            "packs" => counts.packs = value,
            "size-pack" => counts.size_pack = value,
            _ => {}
        }
    }
    Ok(counts)
}

/// Parses `git remote -v`, keeping the fetch line of each remote.
fn parse_remotes(output: &str) -> Vec<Remote> {
    output
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('\t')?;
            let url = rest.strip_suffix(" (fetch)")?;
            Some(Remote {
                name: name.to_string(),
                url: url.to_string(),
            })
        })
        .collect()
}

pub(crate) fn git_in<const N: usize>(path: &Path, args: [&str; N]) -> Command {
    let mut cmd = git();
    cmd.arg("-C").arg(path).args(args);
    cmd
}

/// A `git` command with the repository-locating variables removed, so an
/// exported `GIT_DIR` or similar cannot redirect the operation away from
/// the store. See store-init.md.
fn git() -> Command {
    let mut cmd = Command::new("git");
    for var in REDIRECTING_ENV {
        cmd.env_remove(var);
    }
    cmd
}

const REDIRECTING_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
];

/// Runs `cmd` and returns its stdout.
pub(crate) fn run(mut cmd: Command) -> Result<String, StoreError> {
    let output = cmd.output().map_err(StoreError::Spawn)?;
    if !output.status.success() {
        return Err(StoreError::Git {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `cmd` with `stdin` fed on its standard input, returning stdout.
pub(crate) fn run_with_stdin(mut cmd: Command, stdin: &[u8]) -> Result<String, StoreError> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(StoreError::Spawn)?;
    child
        .stdin
        .as_mut()
        .expect("stdin was requested via Stdio::piped")
        .write_all(stdin)
        .map_err(StoreError::Spawn)?;
    let output = child.wait_with_output().map_err(StoreError::Spawn)?;
    if !output.status.success() {
        return Err(StoreError::Git {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug)]
pub enum StoreError {
    /// No repository at the path
    NotFound(PathBuf),
    /// The `git` binary could not be started
    Spawn(io::Error),
    /// `git` ran and exited with a failure status
    Git { status: ExitStatus, stderr: String },
    /// `git` output did not have the expected shape
    Parse(String),
    /// A `--target` value did not match the `note:<id>` form
    BadTarget(String),
    /// A `--target` referenced a note ref that does not exist
    TargetNotFound(String),
    /// No note ref matched the given id or prefix
    NoteNotFound(String),
    /// More than one note ref matched the given prefix
    AmbiguousNoteId(String, usize),
    /// Operation refused because the note's current commit is a tombstone
    NoteDeleted(String),
    /// No dataset ref matched the given id or prefix
    DatasetNotFound(String),
    /// More than one dataset ref matched the given prefix
    AmbiguousDatasetId(String, usize),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotFound(path) => {
                write!(
                    f,
                    "no Gage store at {} (run `gage store init`)",
                    path.display()
                )
            }
            StoreError::Spawn(e) => write!(f, "failed to run git: {e}"),
            StoreError::Git { status, stderr } => write!(f, "git {status}: {stderr}"),
            StoreError::Parse(what) => write!(f, "unexpected git output: {what}"),
            StoreError::BadTarget(t) => {
                write!(f, "invalid target {t:?}: expected `note:<id>`")
            }
            StoreError::TargetNotFound(t) => write!(f, "target not found: {t}"),
            StoreError::NoteNotFound(id) => write!(f, "note not found: {id}"),
            StoreError::AmbiguousNoteId(id, n) => {
                write!(f, "note id {id} is ambiguous ({n} matches)")
            }
            StoreError::NoteDeleted(id) => write!(f, "note is deleted: {id}"),
            StoreError::DatasetNotFound(id) => write!(f, "dataset not found: {id}"),
            StoreError::AmbiguousDatasetId(id, n) => {
                write!(f, "dataset id {id} is ambiguous ({n} matches)")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Spawn(e) => Some(e),
            StoreError::NotFound(_)
            | StoreError::Git { .. }
            | StoreError::Parse(_)
            | StoreError::BadTarget(_)
            | StoreError::TargetNotFound(_)
            | StoreError::NoteNotFound(_)
            | StoreError::AmbiguousNoteId(_, _)
            | StoreError::NoteDeleted(_)
            | StoreError::DatasetNotFound(_)
            | StoreError::AmbiguousDatasetId(_, _) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_then_reinitializes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("home").join("store.git");

        assert_eq!(init_at(&store).unwrap(), InitOutcome::Created);
        assert_eq!(
            std::fs::read_to_string(store.join("HEAD")).unwrap().trim(),
            "ref: refs/heads/main"
        );
        let config = std::fs::read_to_string(store.join("config")).unwrap();
        assert!(config.contains("bare = true"), "{config}");
        assert!(config.contains("denyDeletes = true"), "{config}");
        assert!(config.contains("denyNonFastForwards = true"), "{config}");
        assert!(config.contains("fsckObjects = true"), "{config}");
        assert!(!store.join("hooks").exists());

        assert_eq!(init_at(&store).unwrap(), InitOutcome::Reinitialized);
    }

    #[test]
    fn init_reports_git_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("occupied");
        std::fs::write(&file, "").unwrap();

        match init_at(&file.join("store.git")) {
            Err(StoreError::Git { stderr, .. }) => assert!(!stderr.is_empty()),
            other => panic!("expected git failure, got {other:?}"),
        }
    }

    #[test]
    fn status_of_fresh_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store.git");
        init_at(&store).unwrap();

        let status = status_at(&store).unwrap();
        assert_eq!(
            status,
            StoreStatus {
                path: store,
                loose_objects: 0,
                packed_objects: 0,
                packs: 0,
                size: 0,
                refs: 0,
                note_refs: 0,
                remotes: vec![],
            }
        );
    }

    #[test]
    fn status_of_missing_store() {
        let tmp = tempfile::tempdir().unwrap();
        match status_at(&tmp.path().join("store.git")) {
            Err(StoreError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn parses_count_objects() {
        let output = "count: 5141\nsize: 67720\nin-pack: 9110\npacks: 4\nsize-pack: 2839\nprune-packable: 26\ngarbage: 0\nsize-garbage: 0\n";
        assert_eq!(
            parse_count_objects(output).unwrap(),
            CountObjects {
                count: 5141,
                size: 67720,
                in_pack: 9110,
                packs: 4,
                size_pack: 2839,
            }
        );
    }

    #[test]
    fn parses_remotes() {
        let output = "origin\tgit@github.com:x/y.git (fetch)\norigin\tgit@github.com:x/y.git (push)\nbackup\t/tmp/b (fetch)\nbackup\t/tmp/b (push)\n";
        let remotes = parse_remotes(output);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].url, "git@github.com:x/y.git");
        assert_eq!(remotes[1].name, "backup");
    }
}
