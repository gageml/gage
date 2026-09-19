//! Thin, generic wrappers over the `git` CLI.
//!
//! Nothing in this module encodes Gage's object model. It is the
//! shell out layer everyone else in the crate calls through. Reads go
//! through one long-lived `git cat-file --batch-command` process per
//! store ([`CatFile`]): a request is a line on its stdin and the reply
//! is the raw object, so a read costs a pipe round-trip rather than a
//! process launch. Writes and administration launch `git` per call.
//! Gage-object concepts live in [`crate::object`].

use std::cell::RefCell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::{Store, StoreError};

/// One entry of a Git tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Six-digit octal mode as `ls-tree` prints it (`100644`, `040000`).
    pub mode: String,
    pub kind: EntryKind,
    pub sha: String,
    /// Bytes for a blob; `None` for a tree.
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
    /// `name <email>`
    pub author: String,
    pub author_time_ms: i64,
    /// `name <email>`
    pub committer: String,
    pub committer_time_ms: i64,
    /// Full commit message, blank line separator intact.
    pub message: String,
}

/// What `cat-file` reports about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub sha: String,
    pub kind: String,
    pub size: u64,
}

/// Attempts per exchange. The first retry covers a killed process; the
/// second covers a `gc` still running across the first retry. More
/// attempts only delay reporting a deterministic fault.
const ATTEMPTS: usize = 3;

/// A `git cat-file --batch-command` reader. `info <name>` returns an
/// object's SHA, kind, and size; `contents <name>` returns those plus
/// the raw bytes. `name` is anything git resolves: a SHA, a ref,
/// `<rev>:<path>`, `<rev>^{tree}`.
///
/// The reader supervises its own child process: a transport fault
/// tears the process down and reruns the request on a fresh one, so
/// callers never see a transient failure. A fault that survives
/// [`ATTEMPTS`], or a process that cannot be started again, is a
/// process fault and panics with git's reason.
pub(crate) struct CatFile {
    path: PathBuf,
    /// `None` only between a teardown and the next spawn.
    process: Option<Process>,
}

impl CatFile {
    pub(crate) fn spawn(path: &Path) -> Result<CatFile, StoreError> {
        Ok(CatFile {
            path: path.to_path_buf(),
            process: Some(Process::spawn(path)?),
        })
    }

    /// `Ok(None)` when git reports the name missing or ambiguous.
    fn info(&mut self, name: &str) -> Result<Option<ObjectInfo>, StoreError> {
        self.exchange("info", name, |process, name| process.read_header(name))
    }

    /// `Ok(None)` when git reports the name missing or ambiguous.
    fn contents(&mut self, name: &str) -> Result<Option<(ObjectInfo, Vec<u8>)>, StoreError> {
        self.exchange("contents", name, |process, name| {
            let Some(info) = process.read_header(name)? else {
                return Ok(None);
            };
            let bytes = process.read_body(info.size)?;
            Ok(Some((info, bytes)))
        })
    }

    /// Runs one request as a supervised exchange: send, then `read` the
    /// reply. Reads are idempotent, so a request that faulted is resent
    /// verbatim on a fresh process.
    fn exchange<T>(
        &mut self,
        command: &str,
        name: &str,
        read: impl Fn(&mut Process, &str) -> Result<T, Fault>,
    ) -> Result<T, StoreError> {
        if name.contains('\n') {
            return Err(StoreError::Parse(format!(
                "object name with newline: {name:?}"
            )));
        }
        let mut reason = String::new();
        for attempt in 1..=ATTEMPTS {
            if attempt > 1 {
                tracing::warn!(
                    attempt,
                    command,
                    name,
                    "git cat-file reader restarted: {reason}"
                );
            }
            let process = self.process.get_or_insert_with(|| {
                Process::spawn(&self.path).unwrap_or_else(|e| {
                    panic!("git cat-file reader could not restart for `{command} {name}`: {e}")
                })
            });
            match process
                .send(command, name)
                .and_then(|()| read(process, name))
            {
                Ok(value) => return Ok(value),
                Err(Fault(what)) => {
                    reason = self.teardown(&what);
                }
            }
        }
        panic!(
            "git cat-file reader failed after {ATTEMPTS} attempts on `{command} {name}`: {reason}"
        )
    }

    /// Ends the current process after a fault and returns the reason:
    /// `what` went wrong at the pipe, plus whatever git wrote to stderr.
    fn teardown(&mut self, what: &str) -> String {
        let Some(process) = self.process.take() else {
            return what.to_string();
        };
        let stderr = process.finish();
        if stderr.is_empty() {
            what.to_string()
        } else {
            format!("{what}: {stderr}")
        }
    }

    #[cfg(test)]
    fn kill_process(&mut self) {
        self.process
            .as_mut()
            .expect("reader is between teardown and spawn")
            .child
            .kill()
            .unwrap();
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            process.finish();
        }
    }
}

/// A failure at the pipe: a write error, EOF, a short body, or a reply
/// out of protocol. Carries a description for the teardown reason.
struct Fault(String);

/// One `git cat-file --batch-command` child and its pipes. Stderr is
/// drained on its own thread into `stderr` for the life of the process:
/// git blocks once the pipe holds 64 KiB unread, and a reader waiting
/// on stdout would then wait forever with no EOF to fault on.
struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
    drain: JoinHandle<()>,
}

impl Process {
    fn spawn(path: &Path) -> Result<Process, StoreError> {
        let mut cmd = git_in(path, ["cat-file", "--batch-command"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(StoreError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .expect("stdin was requested via Stdio::piped");
        let stdout = child
            .stdout
            .take()
            .expect("stdout was requested via Stdio::piped");
        let stderr = child
            .stderr
            .take()
            .expect("stderr was requested via Stdio::piped");
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let drain = thread::spawn({
            let buffer = Arc::clone(&buffer);
            move || drain_stderr(stderr, &buffer)
        });
        Ok(Process {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: buffer,
            drain,
        })
    }

    /// Everything git has written to stderr since the last take.
    fn take_stderr(&self) -> String {
        stderr_text(&self.stderr)
    }

    fn send(&mut self, command: &str, name: &str) -> Result<(), Fault> {
        writeln!(self.stdin, "{command} {name}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| Fault(format!("write failed: {e}")))
    }

    /// `Ok(None)` for a `missing` or `ambiguous` reply, which git
    /// forms by echoing the requested name verbatim; the whole line is
    /// compared so a name containing spaces cannot be mistaken for a
    /// header.
    fn read_header(&mut self, name: &str) -> Result<Option<ObjectInfo>, Fault> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| Fault(format!("read failed: {e}")))?;
        if n == 0 {
            return Err(Fault("process closed its output".to_string()));
        }
        let line = line.strip_suffix('\n').unwrap_or(&line);
        if line == format!("{name} missing") || line == format!("{name} ambiguous") {
            // A corrupt object also reads as missing; git says why on
            // stderr and keeps running.
            let stderr = self.take_stderr();
            if !stderr.is_empty() {
                tracing::warn!(name, "git cat-file: {stderr}");
            }
            return Ok(None);
        }
        let fields: Vec<&str> = line.split(' ').collect();
        match fields.as_slice() {
            [sha, kind, size] => {
                let size = size
                    .parse()
                    .map_err(|e| Fault(format!("reply {line:?}: size: {e}")))?;
                Ok(Some(ObjectInfo {
                    sha: (*sha).to_string(),
                    kind: (*kind).to_string(),
                    size,
                }))
            }
            _ => Err(Fault(format!("reply out of protocol: {line:?}"))),
        }
    }

    /// The object bytes that follow a header, and the newline git
    /// writes after every reply.
    fn read_body(&mut self, size: u64) -> Result<Vec<u8>, Fault> {
        let mut bytes = vec![0u8; size as usize];
        self.stdout
            .read_exact(&mut bytes)
            .map_err(|e| Fault(format!("short body: {e}")))?;
        let mut newline = [0u8; 1];
        self.stdout
            .read_exact(&mut newline)
            .map_err(|e| Fault(format!("short body: {e}")))?;
        Ok(bytes)
    }

    /// Ends the process and returns what it wrote to stderr. Closing
    /// stdin makes git exit at the next read; the kill covers a process
    /// that is alive but out of protocol.
    fn finish(self) -> String {
        let Process {
            mut child,
            stdin,
            stdout: _,
            stderr,
            drain,
        } = self;
        drop(stdin);
        // Err only when the child has already exited, which is the
        // state the kill is meant to reach
        drop(child.kill());
        // Err only when there is no child to wait for, and the kill
        // above has just addressed one
        drop(child.wait());
        let drained = drain.join();
        let text = stderr_text(&stderr);
        match drained {
            Ok(()) => text,
            Err(_) => format!("{text} (stderr drain panicked)"),
        }
    }
}

/// Copies `stderr` into `buffer` until git closes it.
fn drain_stderr(mut stderr: ChildStderr, buffer: &Mutex<Vec<u8>>) {
    if let Err(e) = io::copy(&mut stderr, &mut StderrSink(buffer)) {
        buffer
            .lock()
            .unwrap()
            .extend_from_slice(format!("(stderr unreadable: {e})").as_bytes());
    }
}

/// Everything in `buffer`, taken and decoded, with the surrounding
/// whitespace removed.
fn stderr_text(buffer: &Mutex<Vec<u8>>) -> String {
    let bytes = std::mem::take(&mut *buffer.lock().unwrap());
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// A `Write` over the shared stderr buffer, locking per chunk so the
/// drain thread never holds the lock while blocked on the pipe.
struct StderrSink<'a>(&'a Mutex<Vec<u8>>);

impl Write for StderrSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Store {
    /// What git knows about `name`: SHA, kind, and size. `Ok(None)`
    /// when the name does not resolve.
    pub fn object_info(&self, name: &str) -> Result<Option<ObjectInfo>, StoreError> {
        self.cat_file.borrow_mut().info(name)
    }

    /// The raw bytes of `name` with its info. `Ok(None)` when the name
    /// does not resolve.
    pub fn object_contents(&self, name: &str) -> Result<Option<(ObjectInfo, Vec<u8>)>, StoreError> {
        self.cat_file.borrow_mut().contents(name)
    }

    /// Read a blob's raw bytes. Fails when `spec` does not resolve.
    pub fn read_blob_bytes(&self, spec: &str) -> Result<Vec<u8>, StoreError> {
        match self.object_contents(spec)? {
            Some((_, bytes)) => Ok(bytes),
            None => Err(StoreError::MissingObject(spec.to_string())),
        }
    }

    /// Resolve `reference` to a full SHA. `Ok(None)` when it does not
    /// exist.
    pub(crate) fn rev_parse(&self, reference: &str) -> Result<Option<String>, StoreError> {
        Ok(self.object_info(reference)?.map(|i| i.sha))
    }

    /// The entries directly under `tree_ish`, without sizes. `tree_ish`
    /// is any name that resolves to a tree, or a commit, which is
    /// peeled to its tree.
    pub(crate) fn read_tree(&self, tree_ish: &str) -> Result<Vec<TreeEntry>, StoreError> {
        let (info, mut bytes) = self
            .object_contents(tree_ish)?
            .ok_or_else(|| StoreError::MissingObject(tree_ish.to_string()))?;
        if info.kind == "commit" {
            // A `<rev>:<path>` name cannot carry a `^{tree}` peel, so a
            // commit is peeled here through its own `tree` header.
            let meta = parse_commit(&info.sha, &bytes)
                .map_err(|what| StoreError::Parse(format!("commit {tree_ish}: {what}")))?;
            bytes = self
                .object_contents(&meta.tree)?
                .ok_or_else(|| StoreError::MissingObject(meta.tree.clone()))?
                .1;
        } else if info.kind != "tree" {
            return Err(StoreError::Parse(format!(
                "{tree_ish} is a {}, not a tree",
                info.kind
            )));
        }
        parse_tree(&bytes).map_err(|what| StoreError::Parse(format!("tree {tree_ish}: {what}")))
    }

    /// List entries directly under `tree_or_commit` with blob sizes.
    pub fn list_tree(&self, tree_or_commit: &str) -> Result<Vec<TreeEntry>, StoreError> {
        let mut entries = self.read_tree(tree_or_commit)?;
        for entry in &mut entries {
            if entry.kind == EntryKind::Blob {
                entry.size = self.object_info(&entry.sha)?.map(|i| i.size);
            }
        }
        Ok(entries)
    }

    /// Every blob and tree beneath `reference`, recursively, with blob
    /// sizes. Names are paths relative to `reference`.
    pub fn ls(&self, reference: &str) -> Result<Vec<TreeEntry>, StoreError> {
        let mut out = Vec::new();
        self.walk_tree(reference, "", &mut |entry| {
            out.push(entry);
            Ok(())
        })?;
        Ok(out)
    }

    /// Depth-first walk of every entry beneath `tree_ish`, calling
    /// `visit` with the entry's path relative to `tree_ish` in `name`.
    /// Blob sizes are filled in.
    pub(crate) fn walk_tree(
        &self,
        tree_ish: &str,
        prefix: &str,
        visit: &mut dyn FnMut(TreeEntry) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        for mut entry in self.read_tree(tree_ish)? {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            entry.name = path.clone();
            match entry.kind {
                EntryKind::Tree => {
                    let sha = entry.sha.clone();
                    visit(entry)?;
                    self.walk_tree(&sha, &path, visit)?;
                }
                EntryKind::Blob => {
                    entry.size = self.object_info(&entry.sha)?.map(|i| i.size);
                    visit(entry)?;
                }
                EntryKind::Commit => visit(entry)?,
            }
        }
        Ok(())
    }

    /// Dump the pretty-printed content of `reference` to the process's
    /// stdout by running `git cat-file -p <reference>`.
    pub fn cat(&self, reference: &str) -> Result<(), StoreError> {
        let mut cmd = git_in(self.path(), ["cat-file", "-p", reference]);
        let status = cmd.status().map_err(StoreError::Spawn)?;
        if !status.success() {
            return Err(StoreError::Git {
                status,
                stderr: String::new(),
            });
        }
        Ok(())
    }

    /// Read one commit's metadata. `reference` is any name that
    /// resolves to a commit.
    pub fn read_commit(&self, reference: &str) -> Result<CommitMeta, StoreError> {
        let name = format!("{reference}^{{commit}}");
        let (info, bytes) = self
            .object_contents(&name)?
            .ok_or_else(|| StoreError::MissingObject(reference.to_string()))?;
        parse_commit(&info.sha, &bytes)
            .map_err(|what| StoreError::Parse(format!("commit {reference}: {what}")))
    }
}

/// Parse a raw tree object: repeated `<mode> <name>\0<20-byte sha>`.
fn parse_tree(bytes: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (mode, after_mode) = split_field(rest, b' ').ok_or("entry without mode")?;
        let mode = std::str::from_utf8(mode).map_err(|e| e.to_string())?;
        let (name, after_name) =
            split_field(after_mode, 0).ok_or("entry without name terminator")?;
        let name = String::from_utf8_lossy(name).into_owned();
        let (sha_bytes, after_sha) = after_name.split_at_checked(20).ok_or("entry without sha")?;
        let sha: String = sha_bytes.iter().map(|b| format!("{b:02x}")).collect();
        rest = after_sha;
        let kind = match mode {
            "40000" => EntryKind::Tree,
            "160000" => EntryKind::Commit,
            _ => EntryKind::Blob,
        };
        entries.push(TreeEntry {
            mode: format!("{mode:0>6}"),
            kind,
            sha,
            size: None,
            name,
        });
    }
    Ok(entries)
}

/// Split `bytes` at the first `delimiter`, returning the part before
/// it and the part after it.
fn split_field(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|b| *b == delimiter)?;
    let (before, after) = bytes.split_at_checked(at)?;
    Some((before, after.get(1..)?))
}

/// Parse a raw commit object.
fn parse_commit(sha: &str, bytes: &[u8]) -> Result<CommitMeta, String> {
    let text = String::from_utf8_lossy(bytes);
    let (headers, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    for line in headers.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "tree" => tree = Some(value.to_string()),
            "parent" => parents.push(value.to_string()),
            "author" => author = Some(parse_identity(value)?),
            "committer" => committer = Some(parse_identity(value)?),
            _ => {}
        }
    }
    let (author, author_time_ms) = author.ok_or("missing author")?;
    let (committer, committer_time_ms) = committer.ok_or("missing committer")?;
    Ok(CommitMeta {
        sha: sha.to_string(),
        tree: tree.ok_or("missing tree")?,
        parents,
        author,
        author_time_ms,
        committer,
        committer_time_ms,
        message: message.trim_end_matches('\n').to_string(),
    })
}

/// `name <email> <unix seconds> <tz>` to (`name <email>`, millis).
fn parse_identity(value: &str) -> Result<(String, i64), String> {
    let (ident, tz_rest) = value.rsplit_once(' ').ok_or("identity without timezone")?;
    let _ = tz_rest;
    let (ident, secs) = ident.rsplit_once(' ').ok_or("identity without timestamp")?;
    let secs: i64 = secs
        .parse()
        .map_err(|e| format!("identity timestamp: {e}"))?;
    Ok((ident.to_string(), secs.saturating_mul(1000)))
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

/// Runs `cmd` and returns its stdout. A failing status is an error
/// carrying stderr. Anything git writes to stderr on success, such as
/// a `warning:` line, is logged at warn level so it is not lost.
pub(crate) fn run(mut cmd: Command) -> Result<String, StoreError> {
    let output = cmd.output().map_err(StoreError::Spawn)?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Err(StoreError::Git {
            status: output.status,
            stderr,
        });
    }
    if !stderr.is_empty() {
        tracing::warn!(command = ?cmd, "git: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `cmd` with `stdin` fed on its standard input, returning stdout.
#[cfg(test)]
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

/// The `cat_file` handle type as held by [`Store`].
pub(crate) type CatFileCell = RefCell<CatFile>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init;
    use crate::writer::write_blob;

    fn store_with_blob() -> (tempfile::TempDir, Store, String) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        init(&path).unwrap();
        let sha = write_blob(&path, b"hello").unwrap();
        let store = Store::open(&path).unwrap();
        (tmp, store, sha)
    }

    #[test]
    fn missing_names_with_spaces_are_none_and_reader_stays_usable() {
        let (_tmp, store, sha) = store_with_blob();
        assert_eq!(store.object_info("HEAD:no such thing here").unwrap(), None);
        assert_eq!(store.object_info("a b").unwrap(), None);
        assert_eq!(store.object_info(&sha).unwrap().unwrap().sha, sha);
    }

    #[test]
    fn killed_process_is_replaced_without_the_caller_noticing() {
        let (_tmp, store, sha) = store_with_blob();
        store.cat_file.borrow_mut().kill_process();
        let (info, bytes) = store.object_contents(&sha).unwrap().unwrap();
        assert_eq!(info.sha, sha);
        assert_eq!(bytes, b"hello");
        store.cat_file.borrow_mut().kill_process();
        assert_eq!(store.object_info(&sha).unwrap().unwrap().size, 5);
    }

    #[test]
    fn corrupt_objects_read_as_missing_without_blocking_the_reader() {
        let (_tmp, store, sha) = store_with_blob();
        // Enough corrupt objects that git's stderr exceeds the 64 KiB
        // pipe buffer; an undrained pipe would block git and hang the
        // read.
        let objects = store.path().join("objects");
        let names: Vec<String> = (1..=1000).map(|i| format!("{i:040}")).collect();
        for name in &names {
            let dir = objects.join(&name[..2]);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(&name[2..]), b"garbage").unwrap();
        }
        for name in &names {
            assert_eq!(store.object_info(name).unwrap(), None, "{name}");
        }
        assert_eq!(store.object_info(&sha).unwrap().unwrap().size, 5);
    }

    #[test]
    #[should_panic(expected = "failed after 3 attempts on `contents")]
    fn persistent_fault_is_a_process_fault() {
        let (_tmp, store, sha) = store_with_blob();
        // A truncated loose object: git prints the header, then exits
        // before the body, on every attempt.
        let file = store.path().join("objects").join(&sha[..2]).join(&sha[2..]);
        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..12]).unwrap();
        drop(store.object_contents(&sha));
    }

    #[test]
    fn parse_tree_reads_modes_names_and_shas() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"100644 attrs.json\0");
        bytes.extend_from_slice(&[0xab; 20]);
        bytes.extend_from_slice(b"40000 files\0");
        bytes.extend_from_slice(&[0x01; 20]);
        let entries = parse_tree(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mode, "100644");
        assert_eq!(entries[0].kind, EntryKind::Blob);
        assert_eq!(entries[0].name, "attrs.json");
        assert_eq!(entries[0].sha, "ab".repeat(20));
        assert_eq!(entries[1].mode, "040000");
        assert_eq!(entries[1].kind, EntryKind::Tree);
        assert_eq!(entries[1].name, "files");
    }

    #[test]
    fn parse_commit_reads_headers_and_message() {
        let raw = "tree 1111111111111111111111111111111111111111\n\
                   parent 2222222222222222222222222222222222222222\n\
                   parent 3333333333333333333333333333333333333333\n\
                   author gage <noreply@gage.localhost> 1700000000 +0000\n\
                   committer gage <noreply@gage.localhost> 1700000001 -0500\n\
                   \n\
                   note: comment\n\
                   \n\
                   body\n";
        let meta = parse_commit("abc", raw.as_bytes()).unwrap();
        assert_eq!(meta.sha, "abc");
        assert_eq!(meta.tree, "1".repeat(40));
        assert_eq!(meta.parents, vec!["2".repeat(40), "3".repeat(40)]);
        assert_eq!(meta.author, "gage <noreply@gage.localhost>");
        assert_eq!(meta.author_time_ms, 1_700_000_000_000);
        assert_eq!(meta.committer_time_ms, 1_700_000_001_000);
        assert_eq!(meta.message, "note: comment\n\nbody");
    }
}
