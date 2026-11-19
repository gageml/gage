//! Upstream sources on disk: each run asks upstream for its latest
//! revision and reuses the cached checkout only when it holds exactly
//! that revision, unmodified.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// A source's repository, checked out at its latest revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub dir: PathBuf,
    /// The commit upstream's HEAD names
    pub revision: String,
    /// The checkout was already in the cache
    pub cached: bool,
}

/// Check out the latest revision of the repository at `url` under
/// `cache_dir`. Sources of one repository share a checkout.
pub fn fetch(url: &str, cache_dir: &Path) -> Result<Checkout, Error> {
    let revision = head_revision(url)?;
    let dir = cache_dir.join(cache_name(url));
    if holds(&dir, &revision) {
        return Ok(Checkout {
            dir,
            revision,
            cached: true,
        });
    }
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    git(&dir, &["init", "--quiet"])?;
    git(&dir, &["fetch", "--quiet", "--depth", "1", url, &revision])?;
    git(&dir, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
    Ok(Checkout {
        dir,
        revision,
        cached: false,
    })
}

fn head_revision(url: &str) -> Result<String, Error> {
    let out = git(Path::new("."), &["ls-remote", url, "HEAD"])?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| Error::Other(format!("{url} names no HEAD")))
}

fn cache_name(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Whether `dir` is a checkout of `revision` with no file changed. A
/// directory Git cannot read as a repository holds nothing, and is
/// fetched again.
fn holds(dir: &Path, revision: &str) -> bool {
    if !dir.join(".git").is_dir() {
        return false;
    }
    let head = git(dir, &["rev-parse", "HEAD"]);
    let changes = git(dir, &["status", "--porcelain"]);
    matches!((head, changes), (Ok(head), Ok(changes)) if head.trim() == revision && changes.is_empty())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !out.status.success() {
        return Err(Error::Other(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &Path, file: &str, text: &str) {
        fs::write(repo.join(file), text).unwrap();
        git(repo, &["add", "."]).unwrap();
        git(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--quiet",
                "-m",
                "c",
            ],
        )
        .unwrap();
    }

    #[test]
    fn a_checkout_is_reused_until_upstream_moves_or_the_cache_changes() {
        let upstream = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "--quiet"]).unwrap();
        commit(upstream.path(), "a.jsonl", "one\n");
        let url = upstream.path().to_str().unwrap();

        let first = fetch(url, cache.path()).unwrap();
        assert!(!first.cached);
        assert_eq!(
            fs::read_to_string(first.dir.join("a.jsonl")).unwrap(),
            "one\n"
        );
        assert!(fetch(url, cache.path()).unwrap().cached);

        fs::write(first.dir.join("a.jsonl"), "tampered\n").unwrap();
        let restored = fetch(url, cache.path()).unwrap();
        assert!(!restored.cached);
        assert_eq!(
            fs::read_to_string(restored.dir.join("a.jsonl")).unwrap(),
            "one\n"
        );

        commit(upstream.path(), "a.jsonl", "two\n");
        let moved = fetch(url, cache.path()).unwrap();
        assert!(!moved.cached);
        assert_ne!(moved.revision, first.revision);
        assert_eq!(
            fs::read_to_string(moved.dir.join("a.jsonl")).unwrap(),
            "two\n"
        );
    }
}
