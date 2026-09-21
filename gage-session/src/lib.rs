//! Session driver interface.
//!
//! A driver opens a [`Source`] handle over one container of native
//! sessions (Claude Code on disk, a future OpenCode source, a
//! database). The handle serves the source's tables, id lookup, and
//! project naming, and may cache whatever it reads. A driver writes a
//! native session's bytes into the store through a [`ContentSink`]
//! and reads a stored session back through a [`ContentSource`].

use std::borrow::Cow;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::datasource::TableProvider;

pub trait Driver: Send + Sync {
    /// The driver's name, recorded on stored sessions. Not a scheme.
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;

    /// The source URL schemes this driver serves. A registry routes a
    /// source with one of these schemes to the driver.
    fn schemes(&self) -> &'static [&'static str];

    /// Open a handle over a source. `source` is the value as the user
    /// gave it: empty for the driver's default location, a body under
    /// one of the driver's schemes, or a scheme-less value the driver
    /// interprets as it sees fit. The handle is the driver's place to
    /// hold per-source state (caches, parsed registries, an index) for
    /// as long as the caller keeps it.
    fn open_source(&self, source: &str) -> Result<Box<dyn Source>, DriverError>;

    /// Serialize `session` into the store through `sink`. The returned
    /// string is the `content_format` value the store persists on the
    /// session object; it is opaque to the store and is handed back to
    /// [`Driver::read_stored`] verbatim when the session is read.
    fn write_native(
        &self,
        session: &mut dyn NativeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<String, DriverError>;

    /// Present a stored session for reading.
    fn read_stored(
        &self,
        native_id: String,
        content_format: &str,
        source: Box<dyn ContentSource>,
    ) -> Result<Box<dyn StoredSession>, DriverError>;
}

/// A handle over one source of native sessions. Dropping the handle
/// releases whatever it holds; [`Source::close`] does the same and
/// reports any failure.
pub trait Source: Send + Sync {
    /// The source value the handle was opened with, as given
    fn source(&self) -> &str;

    fn as_any(&self) -> &dyn std::any::Any;

    /// The source's session-derived table providers. Consumers
    /// register them on a DataFusion context; every read pushes
    /// filters, sort, projection, and limit through DataFusion into
    /// the driver's storage.
    fn tables(&self) -> Result<DriverTables, DriverError>;

    /// Expand a typed id or prefix to exactly one native id.
    fn find_native(&self, prefix: &str) -> Result<String, NativeLookupError>;

    fn open_native(&self, native_id: &str) -> Result<Box<dyn NativeSession>, DriverError>;

    /// The project name this driver assigns to a directory. Matches
    /// the `project` column of the `session` table.
    fn project_name(&self, path: &Path) -> Result<String, DriverError>;

    /// The directory a project name denotes, when the source records
    /// one.
    fn project_path(&self, name: &str) -> Result<Option<PathBuf>, DriverError>;

    /// Release the handle.
    fn close(self: Box<Self>) -> Result<(), DriverError>;
}

/// The session-derived tables a driver exposes to gage-query. Every
/// table returned here is registered on the DataFusion context under a
/// well-known name matching its field.
pub struct DriverTables {
    pub session: Arc<dyn TableProvider>,
    pub message: Arc<dyn TableProvider>,
    pub entry: Arc<dyn TableProvider>,
}

pub trait NativeSession {
    fn native_id(&self) -> &str;
    fn session_type(&self) -> &str;
    /// The Gage URL this session was read from, under one of the
    /// driver's schemes. The driver spells it and reads it back; Gage
    /// stores it as the session's `native_source`.
    fn source(&self) -> &str;
    fn attrs(&self) -> &dyn SessionAttrs;
    fn as_any(&self) -> &dyn std::any::Any;
}

pub trait StoredSession {
    fn native_id(&self) -> &str;
    fn content_format(&self) -> &str;
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Box<dyn Entry>, DriverError>> + '_>;
}

/// The attributes of a native session. An implementation decides how
/// and when it reads them; `None` means the harness has no such fact,
/// never that the value was not computed.
pub trait SessionAttrs {
    fn mtime(&self) -> Option<SystemTime>;
    fn size(&self) -> Option<u64>;
    fn is_empty(&self) -> Option<bool>;
    fn project_name(&self) -> Option<&str>;
    fn title(&self) -> Option<&str>;
    fn model(&self) -> Option<&str>;
    fn message_count(&self) -> Option<u64>;
}

/// One raw line of a stored session. Structured reads go through the
/// driver's tables, not through this trait.
pub trait Entry {
    fn line(&self) -> u32;
    fn raw(&self) -> Cow<'_, str>;
}

/// A read-only view of the byte tree a stored session carries.
pub trait ContentSource: Send + Sync {
    fn paths(&self) -> std::io::Result<Vec<String>>;
    fn open(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>>;
}

/// A write-only sink for the byte tree a native session serializes
/// into. `create` returns a writer bound to the sink; only one writer
/// is open at a time.
pub trait ContentSink {
    fn create<'a>(&'a mut self, path: &str) -> std::io::Result<Box<dyn Write + 'a>>;
}

/// Split a source value into `(scheme, body)` when it starts with a
/// URL scheme: an ASCII letter followed by letters, digits, `+`, `-`,
/// or `.`, then a colon. A value with no such prefix has no scheme
/// and is returned as `None`.
pub fn split_scheme(source: &str) -> Option<(&str, &str)> {
    let (scheme, body) = source.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme, body))
}

#[derive(Debug)]
pub enum DriverError {
    Io(std::io::Error),
    Other(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

#[derive(Debug)]
pub enum NativeLookupError {
    NoMatch(String),
    TooManyMatches {
        prefix: String,
        candidates: Vec<String>,
    },
    Driver(DriverError),
}

impl fmt::Display for NativeLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeLookupError::NoMatch(s) => write!(f, "no native session matches {s}"),
            NativeLookupError::TooManyMatches { prefix, candidates } => {
                write!(f, "more than one native session matches {prefix}")?;
                for c in candidates {
                    write!(f, "\n  {c}")?;
                }
                Ok(())
            }
            NativeLookupError::Driver(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for NativeLookupError {}

impl From<DriverError> for NativeLookupError {
    fn from(e: DriverError) -> Self {
        NativeLookupError::Driver(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_scheme_recognizes_scheme_and_body() {
        assert_eq!(
            split_scheme("claude:/tmp/foo"),
            Some(("claude", "/tmp/foo"))
        );
        assert_eq!(split_scheme("claude:"), Some(("claude", "")));
        assert_eq!(
            split_scheme("session+task:x/y"),
            Some(("session+task", "x/y"))
        );
    }

    #[test]
    fn split_scheme_rejects_non_scheme_prefixes() {
        assert_eq!(split_scheme("/tmp/foo"), None);
        assert_eq!(split_scheme(""), None);
        assert_eq!(split_scheme(":body"), None);
        assert_eq!(split_scheme("1abc:x"), None);
        assert_eq!(split_scheme("~/x:y"), None);
    }
}
