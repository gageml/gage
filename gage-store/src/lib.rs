//! Git backed Gage store.
//!
//! Every store operation runs the `git` binary found on `PATH`. No Git
//! library is linked: the store must interoperate with other Git
//! repositories (clone, push, pull), and the binary is the only complete
//! implementation of that surface.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use gage_core::config::gage_home;

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
    let mut cmd = Command::new("git");
    cmd.args([
        "init",
        "--bare",
        "--quiet",
        "--template=",
        "--initial-branch=main",
    ])
    .arg(path);
    run(cmd)?;
    Ok(if existing {
        InitOutcome::Reinitialized
    } else {
        InitOutcome::Created
    })
}

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
    let remotes = parse_remotes(&run(git_in(path, ["remote", "-v"]))?);
    Ok(StoreStatus {
        path: path.to_path_buf(),
        loose_objects: counts.count,
        packed_objects: counts.in_pack,
        packs: counts.packs,
        size: (counts.size + counts.size_pack) * 1024,
        refs,
        remotes,
    })
}

fn exists(path: &Path) -> bool {
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

fn git_in<const N: usize>(path: &Path, args: [&str; N]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path).args(args);
    cmd
}

/// Runs `cmd` and returns its stdout.
fn run(mut cmd: Command) -> Result<String, StoreError> {
    let output = cmd.output().map_err(StoreError::Spawn)?;
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
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Spawn(e) => Some(e),
            StoreError::NotFound(_) | StoreError::Git { .. } | StoreError::Parse(_) => None,
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
