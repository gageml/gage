//! Object writing: blobs, trees, and commits are written as loose
//! objects directly into `objects/`, without a `git` process.
//!
//! A loose object is `zlib(<kind> SP <size> NUL <bytes>)` stored at
//! `objects/<2 hex>/<38 hex>`, named by the SHA-1 of the uncompressed
//! header and bytes. Tree and commit bodies use the same byte layouts
//! [`crate::git`] parses on the read side. Objects are visible to git,
//! and to the store's `cat-file` reader, as soon as the file exists.
//! Ref updates stay with `git update-ref`, which owns ref locking and
//! the compare-and-swap on the old value.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use sha1::{Digest, Sha1};

use crate::StoreError;

/// Fixed identity written to the author and committer of every object
/// commit.
pub(crate) const IDENTITY_NAME: &str = "gage";
pub(crate) const IDENTITY_EMAIL: &str = "noreply@gage.localhost";

/// One entry to write into a tree.
pub(crate) struct TreeInput<'a> {
    /// Six-digit octal mode (`100644`, `040000`).
    pub mode: &'a str,
    pub sha: &'a str,
    pub name: &'a str,
}

/// Write `content` as a blob and return its SHA.
pub(crate) fn write_blob(path: &Path, content: &[u8]) -> Result<String, StoreError> {
    write_object(path, "blob", content)
}

/// Read `content` to its end and write it as a blob. The whole
/// content is held in memory to compute its SHA, as `git hash-object`
/// does for a stream of unknown length.
pub(crate) fn write_blob_stream(path: &Path, mut content: impl Read) -> Result<String, StoreError> {
    let mut bytes = Vec::new();
    content.read_to_end(&mut bytes).map_err(StoreError::Spawn)?;
    write_object(path, "blob", &bytes)
}

/// Write a tree from `entries`, in any order, and return its SHA.
pub(crate) fn mktree(path: &Path, entries: &[TreeInput<'_>]) -> Result<String, StoreError> {
    let mut sorted: Vec<&TreeInput<'_>> = entries.iter().collect();
    sorted.sort_by_key(|entry| tree_sort_key(entry));
    let mut body = Vec::new();
    for entry in sorted {
        let mode = entry.mode.trim_start_matches('0');
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(entry.name.as_bytes());
        body.push(0);
        body.extend_from_slice(&decode_sha(entry.sha)?);
    }
    write_object(path, "tree", &body)
}

/// Git orders tree entries by name bytes, comparing a directory as if
/// its name ended in `/`.
fn tree_sort_key(entry: &TreeInput<'_>) -> Vec<u8> {
    let mut key = entry.name.as_bytes().to_vec();
    if entry.mode.trim_start_matches('0') == "40000" {
        key.push(b'/');
    }
    key
}

/// Write a commit for `tree_sha` under the fixed Gage identity with
/// `message`, and return its SHA. `parents` are written in order: the
/// lineage parent (if any) first, link SHAs after. An empty slice
/// produces a parentless commit.
pub(crate) fn commit_tree(
    path: &Path,
    tree_sha: &str,
    message: &str,
    parents: &[&str],
) -> Result<String, StoreError> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    commit_tree_at(path, tree_sha, message, parents, secs)
}

/// [`commit_tree`] with an explicit author and committer time in UNIX
/// seconds, so a test can pin the bytes.
fn commit_tree_at(
    path: &Path,
    tree_sha: &str,
    message: &str,
    parents: &[&str],
    secs: u64,
) -> Result<String, StoreError> {
    let identity = format!("{IDENTITY_NAME} <{IDENTITY_EMAIL}> {secs} +0000");
    let mut body = String::new();
    body.push_str(&format!("tree {tree_sha}\n"));
    for parent in parents {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str(&format!("author {identity}\n"));
    body.push_str(&format!("committer {identity}\n"));
    body.push('\n');
    body.push_str(message);
    if !message.ends_with('\n') {
        body.push('\n');
    }
    write_object(path, "commit", body.as_bytes())
}

/// Write one loose object and return its SHA. An object that already
/// exists is left in place; git's content addressing makes the write
/// a no-op.
fn write_object(path: &Path, kind: &str, bytes: &[u8]) -> Result<String, StoreError> {
    let mut hasher = Sha1::new();
    hasher.update(format!("{kind} {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    let sha: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (dir_name, file_name) = sha.split_at(2);
    let dir = path.join("objects").join(dir_name);
    let file = dir.join(file_name);
    if file.exists() {
        return Ok(sha);
    }
    fs::create_dir_all(&dir).map_err(StoreError::Spawn)?;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(format!("{kind} {}\0", bytes.len()).as_bytes())
        .map_err(StoreError::Spawn)?;
    encoder.write_all(bytes).map_err(StoreError::Spawn)?;
    let compressed = encoder.finish().map_err(StoreError::Spawn)?;

    // Write beside the target and rename, so a reader never sees a
    // partial object. Two writers racing on the same object produce
    // identical bytes, so either rename winning is correct.
    let tmp = dir.join(format!("tmp_{}_{}", std::process::id(), file_name));
    fs::write(&tmp, &compressed).map_err(StoreError::Spawn)?;
    fs::rename(&tmp, &file).map_err(StoreError::Spawn)?;
    Ok(sha)
}

/// Decode a 40-character hex SHA into its 20 bytes.
fn decode_sha(sha: &str) -> Result<[u8; 20], StoreError> {
    let bytes = sha.as_bytes();
    if bytes.len() != 40 {
        return Err(StoreError::Parse(format!(
            "sha {sha:?} is not 40 hex chars"
        )));
    }
    let mut out = [0u8; 20];
    for (i, slot) in out.iter_mut().enumerate() {
        let pair = sha
            .get(i * 2..i * 2 + 2)
            .ok_or_else(|| StoreError::Parse(format!("sha {sha:?} is not 40 hex chars")))?;
        *slot = u8::from_str_radix(pair, 16)
            .map_err(|e| StoreError::Parse(format!("sha {sha:?}: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{git_in, run};
    use crate::init;

    fn repo(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("store.git");
        init(&path).unwrap();
        path
    }

    fn git_hash(path: &Path, kind: &str, bytes: &[u8]) -> String {
        crate::git::run_with_stdin(git_in(path, ["hash-object", "-t", kind, "--stdin"]), bytes)
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn blob_sha_matches_git_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let sha = write_blob(&path, b"hello\n").unwrap();
        assert_eq!(sha, git_hash(&path, "blob", b"hello\n"));
        assert_eq!(
            run(git_in(&path, ["cat-file", "-p", &sha])).unwrap(),
            "hello\n"
        );
        assert_eq!(
            run(git_in(&path, ["cat-file", "-t", &sha])).unwrap().trim(),
            "blob"
        );
    }

    #[test]
    fn tree_sorts_like_git_and_passes_fsck() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let blob = write_blob(&path, b"x").unwrap();
        let inner = mktree(
            &path,
            &[TreeInput {
                mode: "100644",
                sha: &blob,
                name: "a",
            }],
        )
        .unwrap();
        // `b` as a directory sorts after `b.txt` because git compares
        // it as `b/`; `b-` sorts before both.
        let tree = mktree(
            &path,
            &[
                TreeInput {
                    mode: "100644",
                    sha: &blob,
                    name: "b.txt",
                },
                TreeInput {
                    mode: "040000",
                    sha: &inner,
                    name: "b",
                },
                TreeInput {
                    mode: "100644",
                    sha: &blob,
                    name: "b-",
                },
            ],
        )
        .unwrap();
        let listing = run(git_in(&path, ["ls-tree", "--name-only", &tree])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["b-", "b.txt", "b"]
        );
        run(git_in(&path, ["fsck", "--strict"])).unwrap();
    }

    #[test]
    fn commit_carries_identity_parents_and_message() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let blob = write_blob(&path, b"x").unwrap();
        let tree = mktree(
            &path,
            &[TreeInput {
                mode: "100644",
                sha: &blob,
                name: "a",
            }],
        )
        .unwrap();
        let first = commit_tree(&path, &tree, "one", &[]).unwrap();
        let second = commit_tree(&path, &tree, "two\n\nbody", &[&first]).unwrap();
        let raw = run(git_in(&path, ["cat-file", "-p", &second])).unwrap();
        assert!(raw.contains(&format!("tree {tree}\n")), "{raw}");
        assert!(raw.contains(&format!("parent {first}\n")), "{raw}");
        assert!(
            raw.contains("author gage <noreply@gage.localhost> "),
            "{raw}"
        );
        assert!(raw.ends_with("two\n\nbody\n"), "{raw}");
        run(git_in(&path, ["update-ref", "refs/heads/t", &second])).unwrap();
        run(git_in(&path, ["fsck", "--strict"])).unwrap();
    }

    /// The same tree built by `git mktree` from the same entries, given
    /// in a different order.
    #[test]
    fn tree_sha_matches_git_mktree() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let blob = write_blob(&path, b"x").unwrap();
        let inner = mktree(
            &path,
            &[TreeInput {
                mode: "100644",
                sha: &blob,
                name: "z",
            }],
        )
        .unwrap();
        let ours = mktree(
            &path,
            &[
                TreeInput {
                    mode: "100644",
                    sha: &blob,
                    name: "b.txt",
                },
                TreeInput {
                    mode: "040000",
                    sha: &inner,
                    name: "b",
                },
                TreeInput {
                    mode: "100644",
                    sha: &blob,
                    name: "a",
                },
            ],
        )
        .unwrap();
        let input =
            format!("100644 blob {blob}\ta\n040000 tree {inner}\tb\n100644 blob {blob}\tb.txt\n");
        let theirs = crate::git::run_with_stdin(git_in(&path, ["mktree"]), input.as_bytes())
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(ours, theirs);
    }

    /// The same commit built by `git commit-tree` with the same tree,
    /// parents, message, identity, and time.
    #[test]
    fn commit_sha_matches_git_commit_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let blob = write_blob(&path, b"x").unwrap();
        let tree = mktree(
            &path,
            &[TreeInput {
                mode: "100644",
                sha: &blob,
                name: "a",
            }],
        )
        .unwrap();
        let secs = 1_700_000_000u64;
        let parent = commit_tree_at(&path, &tree, "one", &[], secs).unwrap();
        let ours = commit_tree_at(&path, &tree, "two\n\nbody", &[&parent], secs).unwrap();

        let date = format!("{secs} +0000");
        let mut cmd = git_in(
            &path,
            ["commit-tree", &tree, "-p", &parent, "-m", "two\n\nbody"],
        );
        cmd.env("GIT_AUTHOR_NAME", IDENTITY_NAME)
            .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
            .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL)
            .env("GIT_COMMITTER_DATE", &date);
        let theirs = run(cmd).unwrap().trim().to_string();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn existing_object_is_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let sha = write_blob(&path, b"same").unwrap();
        let file = path.join("objects").join(&sha[..2]).join(&sha[2..]);
        let before = fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(write_blob(&path, b"same").unwrap(), sha);
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), before);
    }
}
