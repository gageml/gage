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
use tempfile::NamedTempFile;

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
    content
        .read_to_end(&mut bytes)
        .map_err(StoreError::ReadContent)?;
    write_object(path, "blob", &bytes)
}

/// Write a tree from `entries`, in any order, and return its SHA.
pub(crate) fn mktree(path: &Path, entries: &[TreeInput<'_>]) -> Result<String, StoreError> {
    let mut sorted: Vec<&TreeInput<'_>> = entries.iter().collect();
    sorted.sort_by_key(|entry| tree_sort_key(entry));
    let mut body = Vec::new();
    for entry in sorted {
        validate_tree_name(entry.name)?;
        let mode = entry.mode.trim_start_matches('0');
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(entry.name.as_bytes());
        body.push(0);
        body.extend_from_slice(&decode_sha(entry.sha)?);
    }
    write_object(path, "tree", &body)
}

/// Rejects tree entry names git's own parser or `fsck --strict` would
/// choke on. `/` is rejected because a tree entry names one path
/// component; a `/` inside an entry produces a legal-but-flat tree
/// where git expects a subtree.
fn validate_tree_name(name: &str) -> Result<(), StoreError> {
    let reason = if name.is_empty() {
        "empty tree entry name"
    } else if name.contains('\0') {
        "tree entry name contains NUL"
    } else if name.contains('/') {
        "tree entry name contains /"
    } else if name == "." || name == ".." {
        "tree entry name is `.` or `..`"
    } else if is_dot_git(name) {
        "tree entry name is `.git`"
    } else {
        return Ok(());
    };
    Err(StoreError::InvalidPath {
        path: name.to_string(),
        reason: reason.to_string(),
    })
}

/// True when git's `fsck` treats `name` as `.git`, which a receiving
/// store rejects under `transfer.fsckObjects`. Mirrors git's
/// `is_hfs_dotgit` and `is_ntfs_dotgit`: the comparison is
/// case-insensitive, ignores the code points HFS+ drops from names,
/// ignores the trailing spaces and dots NTFS drops, and includes the
/// NTFS short name `git~1`.
pub(crate) fn is_dot_git(name: &str) -> bool {
    let visible: String = name.chars().filter(|c| !is_hfs_ignored(*c)).collect();
    let visible = visible.trim_end_matches([' ', '.']);
    visible.eq_ignore_ascii_case(".git") || visible.eq_ignore_ascii_case("git~1")
}

/// Code points HFS+ ignores when comparing file names, per git's
/// `is_hfs_ignored_codepoint`.
fn is_hfs_ignored(c: char) -> bool {
    matches!(
        c,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
    )
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
    let write = |source| StoreError::Write {
        path: file.clone(),
        source,
    };
    fs::create_dir_all(&dir).map_err(write)?;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(format!("{kind} {}\0", bytes.len()).as_bytes())
        .map_err(write)?;
    encoder.write_all(bytes).map_err(write)?;
    let compressed = encoder.finish().map_err(write)?;

    // Write to a uniquely named file beside the target and rename it
    // into place, so a reader never sees a partial object and two
    // writers never share a temporary path. Two writers racing on the
    // same object produce identical bytes, so either rename winning is
    // correct. A failed write drops the temporary file with it.
    let mut tmp = NamedTempFile::new_in(&dir).map_err(write)?;
    tmp.write_all(&compressed).map_err(write)?;
    tmp.persist(&file).map_err(|e| write(e.error))?;
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
    fn dot_git_forms_match_what_fsck_rejects() {
        // Each row was checked by pushing a raw tree into a store with
        // transfer.fsckObjects on git 2.43.
        for name in [
            ".git", ".GIT", ".Git", ".git.", ".git ", ".git..", "git~1", "GIT~1",
        ] {
            assert!(is_dot_git(name), "{name:?}");
        }
        for cp in [
            '\u{200c}', '\u{200f}', '\u{202a}', '\u{202e}', '\u{206a}', '\u{206f}', '\u{feff}',
        ] {
            assert!(is_dot_git(&format!(".g{cp}it")), "U+{:04X}", cp as u32);
        }
        for name in [".gitx", ".gitmodules", "GIT~10", "git", "a.git"] {
            assert!(!is_dot_git(name), "{name:?}");
        }
        for cp in [
            '\u{200b}', '\u{2010}', '\u{2029}', '\u{202f}', '\u{2069}', '\u{2070}', '\u{2060}',
            '\u{fefe}',
        ] {
            assert!(!is_dot_git(&format!(".g{cp}it")), "U+{:04X}", cp as u32);
        }
    }

    #[test]
    fn mktree_rejects_invalid_names() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let blob = write_blob(&path, b"x").unwrap();
        let cases = [
            ("", "empty tree entry name"),
            ("a\0b", "tree entry name contains NUL"),
            ("a/b", "tree entry name contains /"),
            (".", "tree entry name is `.` or `..`"),
            ("..", "tree entry name is `.` or `..`"),
            (".git", "tree entry name is `.git`"),
            (".GIT", "tree entry name is `.git`"),
            (".git.", "tree entry name is `.git`"),
            (".git ", "tree entry name is `.git`"),
            (".git . ", "tree entry name is `.git`"),
            ("git~1", "tree entry name is `.git`"),
            ("GIT~1", "tree entry name is `.git`"),
            (".g\u{200c}it", "tree entry name is `.git`"),
            (".git\u{feff}", "tree entry name is `.git`"),
        ];
        for (name, expected) in cases {
            let result = mktree(
                &path,
                &[TreeInput {
                    mode: "100644",
                    sha: &blob,
                    name,
                }],
            );
            match result {
                Err(StoreError::InvalidPath { path: p, reason }) => {
                    assert_eq!(p, name, "{name:?}");
                    assert_eq!(reason, expected, "{name:?}");
                }
                other => panic!("expected InvalidPath for {name:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn write_leaves_no_temporary_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let sha = write_blob(&path, b"tidy").unwrap();
        let dir = path.join("objects").join(&sha[..2]);
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![sha[2..].to_string()]);
    }

    #[test]
    fn concurrent_writers_of_one_object_both_succeed() {
        // A smoke check, not a proof: the race is not deterministic.
        // What it pins is that no writer reports failure and the object
        // decodes, which the shared per-pid temporary path violated.
        let tmp = tempfile::tempdir().unwrap();
        let path = repo(tmp.path());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    (0..50)
                        .map(|i| write_blob(&path, format!("shared {i}").as_bytes()).unwrap())
                        .collect::<Vec<String>>()
                })
            })
            .collect();
        let mut shas: Vec<Vec<String>> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        let first = shas.remove(0);
        assert!(shas.iter().all(|s| *s == first));
        for (i, sha) in first.iter().enumerate() {
            let expected = format!("shared {i}");
            let out = run(git_in(&path, ["cat-file", "-p", sha])).unwrap();
            assert_eq!(out, expected, "{sha}");
        }
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
