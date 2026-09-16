//! Thin, generic wrappers over the `git` CLI.
//!
//! Nothing in this module encodes Gage's object model. It is the
//! shell out layer everyone else in the crate calls through: launch a
//! `git` command in the right directory with the right environment
//! sanitised, and parse the shapes the CLI produces (tree listings,
//! commit objects, blob bytes). Gage-object concepts live in
//! [`crate::object`].

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::StoreError;

/// One entry from `git ls-tree -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: EntryKind,
    pub sha: String,
    /// Bytes on disk for a blob; `None` for a tree.
    pub size: Option<u64>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Parsed Git commit metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_time_ms: i64,
    pub committer: String,
    pub committer_time_ms: i64,
    /// Full commit message, blank line separator intact.
    pub message: String,
}

/// List the entries directly under `reference`, resolved by
/// `git ls-tree -l`. `reference` is a git tree-ish (ref path, sha, or
/// `<ref>:<path>`). Recursive: every blob and tree beneath the given
/// tree-ish is included.
pub fn ls(reference: &str) -> Result<Vec<TreeEntry>, StoreError> {
    ls_at(&crate::store_path(), reference)
}

pub fn ls_at(path: &Path, reference: &str) -> Result<Vec<TreeEntry>, StoreError> {
    if !crate::exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let out = run(git_in(path, ["ls-tree", "-r", "-t", "-l", reference]))?;
    parse_ls_tree(&out)
}

/// List entries directly under `tree_or_commit`, non-recursively.
pub fn list_tree(tree_or_commit: &str) -> Result<Vec<TreeEntry>, StoreError> {
    list_tree_at(&crate::store_path(), tree_or_commit)
}

pub fn list_tree_at(path: &Path, tree_or_commit: &str) -> Result<Vec<TreeEntry>, StoreError> {
    if !crate::exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let out = run(git_in(path, ["ls-tree", "-l", tree_or_commit]))?;
    parse_ls_tree(&out)
}

fn parse_ls_tree(out: &str) -> Result<Vec<TreeEntry>, StoreError> {
    let mut entries = Vec::new();
    for line in out.lines() {
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind_str, sha, size_str]: [&str; 4] =
            fields.try_into().map_err(|got: Vec<&str>| {
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
    cat_at(&crate::store_path(), reference)
}

pub fn cat_at(path: &Path, reference: &str) -> Result<(), StoreError> {
    if !crate::exists(path) {
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

/// Read a blob's raw bytes via `git cat-file -p <spec>`.
pub fn read_blob_bytes(spec: &str) -> Result<Vec<u8>, StoreError> {
    read_blob_bytes_at(&crate::store_path(), spec)
}

pub fn read_blob_bytes_at(path: &Path, spec: &str) -> Result<Vec<u8>, StoreError> {
    let mut cmd = git_in(path, ["cat-file", "-p", spec]);
    let output = cmd.output().map_err(StoreError::Spawn)?;
    if !output.status.success() {
        return Err(StoreError::Git {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

/// Read one commit's metadata. `sha` is any commit reference the git
/// CLI accepts.
pub fn read_commit(sha: &str) -> Result<CommitMeta, StoreError> {
    read_commit_at(&crate::store_path(), sha)
}

pub fn read_commit_at(path: &Path, sha: &str) -> Result<CommitMeta, StoreError> {
    // A null-terminated separator between the header line group and
    // the message would be safer, but keeping the message at the end
    // and using `splitn(8, '\n')` is sufficient: only the message can
    // contain embedded newlines.
    let format = "%H%n%T%n%P%n%an <%ae>%n%at%n%cn <%ce>%n%ct%n%B";
    let raw = run(git_in(
        path,
        [
            "log",
            "-1",
            "--no-decorate",
            &format!("--format={format}"),
            sha,
        ],
    ))?;
    let mut lines = raw.splitn(8, '\n');
    let sha = next_field(&mut lines, "sha")?.to_string();
    let tree = next_field(&mut lines, "tree")?.to_string();
    let parents_line = next_field(&mut lines, "parents")?;
    let parents: Vec<String> = parents_line
        .split_ascii_whitespace()
        .map(String::from)
        .collect();
    let author = next_field(&mut lines, "author")?.to_string();
    let author_time_ms = parse_secs(next_field(&mut lines, "author time")?)?;
    let committer = next_field(&mut lines, "committer")?.to_string();
    let committer_time_ms = parse_secs(next_field(&mut lines, "committer time")?)?;
    let message = lines
        .next()
        .unwrap_or("")
        .trim_end_matches('\n')
        .to_string();
    Ok(CommitMeta {
        sha,
        tree,
        parents,
        author,
        author_time_ms,
        committer,
        committer_time_ms,
        message,
    })
}

fn next_field<'a>(
    lines: &mut std::str::SplitN<'a, char>,
    what: &str,
) -> Result<&'a str, StoreError> {
    lines
        .next()
        .ok_or_else(|| StoreError::Parse(format!("commit log: missing {what}")))
}

fn parse_secs(text: &str) -> Result<i64, StoreError> {
    let secs: i64 = text
        .parse()
        .map_err(|e| StoreError::Parse(format!("commit time {text:?}: {e}")))?;
    Ok(secs.saturating_mul(1000))
}

/// A `git` command against the repository at `path`, with `-C <path>`
/// and the requested arguments applied.
pub(crate) fn git_in<const N: usize>(path: &Path, args: [&str; N]) -> Command {
    let mut cmd = git_cmd();
    cmd.arg("-C").arg(path).args(args);
    cmd
}

/// A `git` command with the repository-locating variables removed, so
/// an exported `GIT_DIR` or similar cannot redirect the operation away
/// from the store. See `store-init.md`.
pub(crate) fn git_cmd() -> Command {
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
