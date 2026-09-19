//! Session driver interface.
//!
//! A driver enumerates and reads sessions from one source (Claude Code
//! on disk, a future codex source, a database). It exposes a
//! [`SourceSession`] whose `files` method streams the session's content
//! into a dataset without materializing it in memory.
//!
//! `StoreSession` will be the read-side counterpart: a session already
//! stored in a dataset presented back to a consumer. It is not defined
//! here yet; it will land when the first consumer of stored sessions
//! is written.

use std::fmt;
use std::io::Read;

/// A driver reads sessions from a source keyed by a URL scheme and
/// opens sessions already stored in a dataset.
pub trait Driver: Send + Sync {
    /// The URL scheme this driver responds to, e.g. `"claude"`.
    fn name(&self) -> &'static str;

    /// The driver's own version, e.g. `"0.2.0"`. Recorded as
    /// `"{name} {version}"` in a dataset's session `driver` file.
    fn version(&self) -> &'static str;

    /// Resolve `id` (the part after `<name>:` in a spec) into a
    /// [`SourceSession`].
    fn resolve(&self, id: &str) -> Result<Box<dyn SourceSession>, DriverError>;

    /// Open a stored session for reading. `access` is scoped to the
    /// session's `content/` subtree; `session_id`, `session_type`, and
    /// `content_format` are the metadata files' contents that live
    /// outside `content/`.
    fn open(
        &self,
        session_id: String,
        session_type: SessionType,
        content_format: Option<String>,
        access: Box<dyn ContentAccess>,
    ) -> Result<Box<dyn StoreSession>, DriverError>;
}

/// Facts about a native session that only its driver can know: what
/// the session is called, which model produced it, how many messages
/// it holds, and how large its own files are. The store writes them
/// into the session object as `attrs.summary` at add time so that
/// listings never parse content. Every field is optional; a driver
/// reports what it can.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    pub title: Option<String>,
    pub model: Option<String>,
    pub message_count: Option<u64>,
    /// Bytes of the session's own files, as the driver measures them.
    pub size: Option<u64>,
}

/// A view of one session ready to be written into a dataset.
pub trait SourceSession {
    /// The source-assigned id, preserved verbatim.
    fn session_id(&self) -> &str;

    /// The session type: name + version, dispatch key for parsers.
    fn session_type(&self) -> &SessionType;

    /// Optional storage format hint for the session's `content/`
    /// files. `None` when the session type is enough on its own.
    fn content_format(&self) -> Option<&str>;

    /// The driver's projection of the session, stored as
    /// `attrs.summary`. Nothing outside the driver can compute it.
    fn summary(&self) -> SessionSummary;

    /// Stream the session's content files. Called once. Each yielded
    /// [`SessionFile`] carries a relative path (under `content/`) and
    /// a stream of bytes. Order is not significant; the writer sorts.
    fn files(&mut self) -> Box<dyn Iterator<Item = Result<SessionFile, DriverError>> + '_>;
}

/// Access to the files under a stored session's `content/` subtree.
/// Implementations back this with git blob reads.
pub trait ContentAccess: Send + Sync {
    /// Every path (relative to `content/`) that exists in the session,
    /// in an unspecified order. Forward-slash separated.
    fn paths(&self) -> std::io::Result<Vec<String>>;

    /// Open one path for streaming reads.
    fn open(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>>;
}

/// A stored session presented for reading. The driver produces one
/// normalized view over its native storage: `entries()` yields the
/// event stream row-by-row.
pub trait StoreSession {
    fn session_id(&self) -> &str;
    fn session_type(&self) -> &SessionType;
    fn content_format(&self) -> Option<&str>;

    /// Row iterator over the session's raw event stream, one row per
    /// source line. Called once.
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Entry, DriverError>> + '_>;
}

/// One row of the entry table's raw view. Minimal for now; more
/// columns (uuid, type, subtype, timestamp) are added as consumers
/// need them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based line number in the source content file.
    pub line: u32,
    /// The source line's bytes as a UTF-8 string.
    pub raw: String,
}

/// One file in a session, streamed.
pub struct SessionFile {
    /// Path relative to `content/`, forward-slash separated. May contain
    /// sub-directories.
    pub path: String,
    /// The file's bytes, streamed on demand.
    pub content: Box<dyn Read + Send>,
}

/// The parser dispatch key and its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionType {
    pub name: String,
    pub version: String,
}

impl SessionType {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

impl fmt::Display for SessionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.version)
    }
}

/// Failures returned by a driver.
#[derive(Debug)]
pub enum DriverError {
    /// No session matched the given id at this source.
    SessionNotFound(String),
    /// An I/O error reading the source.
    Io(std::io::Error),
    /// Anything else the driver wants to surface.
    Other(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::SessionNotFound(id) => write!(f, "session not found: {id}"),
            DriverError::Io(e) => write!(f, "io: {e}"),
            DriverError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for DriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DriverError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DriverError {
    fn from(e: std::io::Error) -> Self {
        DriverError::Io(e)
    }
}
