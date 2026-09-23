//! Read access to the byte tree a stored session version carries.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

use gage_session::ContentSource;

use crate::git::{git_in, run};

/// A [`ContentSource`] over `files.d/` of one session commit, read
/// through the `git` binary.
pub(crate) struct GitContentSource {
    store_path: PathBuf,
    session_commit: String,
}

impl GitContentSource {
    pub(crate) fn new(store_path: PathBuf, session_commit: String) -> Self {
        Self {
            store_path,
            session_commit,
        }
    }
}

impl ContentSource for GitContentSource {
    fn paths(&self) -> io::Result<Vec<String>> {
        // `-z` prints names raw with NUL separators. Without it git
        // C-quotes any name with a byte >= 0x80, a tab, a backslash, a
        // quote, or a newline, and the quoted form is not a path.
        let listing = run(git_in(
            &self.store_path,
            [
                "ls-tree",
                "-r",
                "-z",
                "--name-only",
                &format!("{}:files.d", self.session_commit),
            ],
        ))
        .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(listing
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect())
    }

    fn open(&self, path: &str) -> io::Result<Box<dyn Read + Send>> {
        let target = format!("{}:files.d/{}", self.session_commit, path);
        let mut cmd: Command = git_in(&self.store_path, ["cat-file", "-p", &target]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .expect("stdout was requested via Stdio::piped");
        Ok(Box::new(GitReader {
            child,
            stdout,
            finished: false,
        }))
    }
}

struct GitReader {
    child: Child,
    stdout: ChildStdout,
    /// Set once the process has been waited on at end of stream.
    finished: bool,
}

impl Read for GitReader {
    /// At end of stream the process is reaped, and a failing status
    /// becomes an error carrying git's stderr, so a truncated read is
    /// never mistaken for a complete one.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        if n == 0 && !self.finished {
            self.finished = true;
            let status = self.child.wait()?;
            if !status.success() {
                let mut reason = String::new();
                if let Some(stderr) = self.child.stderr.as_mut() {
                    // Best-effort: the stderr snapshot may be truncated
                    // or empty. The exit status in the error message
                    // carries git's own reason on its own.
                    drop(stderr.read_to_string(&mut reason));
                }
                return Err(io::Error::other(format!(
                    "git cat-file {status}: {}",
                    reason.trim()
                )));
            }
        }
        Ok(n)
    }
}

impl Drop for GitReader {
    fn drop(&mut self) {
        // Reap the git process to avoid a zombie. A reader dropped
        // before end of stream has nothing to report.
        if !self.finished {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }
}
