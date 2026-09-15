//! Shared plumbing for artifact writers: writing blobs, building trees,
//! and committing under the fixed Gage identity.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{StoreError, git_in, run, run_with_stdin};

/// Fixed identity written to the author and committer of every artifact
/// commit.
pub(crate) const IDENTITY_NAME: &str = "gage";
pub(crate) const IDENTITY_EMAIL: &str = "noreply@gage.localhost";

/// Write `content` as a blob via `git hash-object -w --stdin` and return
/// its sha. For metadata and other small buffers.
pub(crate) fn write_blob(path: &Path, content: &[u8]) -> Result<String, StoreError> {
    let sha = run_with_stdin(git_in(path, ["hash-object", "-w", "--stdin"]), content)?;
    Ok(sha.trim().to_string())
}

/// Stream `content` into `git hash-object -w --stdin` and return the
/// blob sha. Memory is bounded by the copy buffer, so this is the right
/// choice for driver-supplied session content that may be large.
pub(crate) fn write_blob_stream(path: &Path, mut content: impl Read) -> Result<String, StoreError> {
    let mut cmd = git_in(path, ["hash-object", "-w", "--stdin"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(StoreError::Spawn)?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .expect("stdin was requested via Stdio::piped");
        std::io::copy(&mut content, stdin).map_err(StoreError::Spawn)?;
    }
    let output = child.wait_with_output().map_err(StoreError::Spawn)?;
    if !output.status.success() {
        return Err(StoreError::Git {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Feed `entries` (already in git tree byte-sort order) to `git mktree`
/// and return the resulting tree sha.
pub(crate) fn mktree(path: &Path, entries: &[String]) -> Result<String, StoreError> {
    let mut input = entries.join("\n");
    input.push('\n');
    let sha = run_with_stdin(git_in(path, ["mktree"]), input.as_bytes())?;
    Ok(sha.trim().to_string())
}

/// Create a commit for `tree_sha` under the fixed Gage identity with
/// `message`. `parent = None` produces a parentless commit; `Some(sha)`
/// chains against the previous ref value.
pub(crate) fn commit_tree(
    path: &Path,
    tree_sha: &str,
    message: &str,
    parent: Option<&str>,
) -> Result<String, StoreError> {
    let mut cmd: Command = git_in(path, ["commit-tree", tree_sha]);
    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }
    cmd.arg("-m").arg(message);
    cmd.env("GIT_AUTHOR_NAME", IDENTITY_NAME)
        .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
        .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
        .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL);
    Ok(run(cmd)?.trim().to_string())
}
