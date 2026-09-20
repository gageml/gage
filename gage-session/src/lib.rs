//! Session driver interface.
//!
//! A driver enumerates and reads sessions from one source (Claude Code
//! on disk, a future OpenCode source, a database). It writes a native
//! session's bytes into the store through a [`ContentSink`] and reads
//! a stored session back through a [`ContentSource`].

use std::borrow::Cow;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::datasource::TableProvider;

pub trait Driver: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;

    /// Return the driver's session-derived table providers, bound to
    /// `source`. Consumers register the returned providers on a
    /// DataFusion context; every read pushes filters, sort, projection,
    /// and limit through DataFusion into the driver's storage.
    fn tables(&self, source: &SourceUrl) -> Result<DriverTables, DriverError>;

    fn find_native(&self, source: &SourceUrl, prefix: &str) -> Result<String, NativeLookupError>;

    fn open_native(
        &self,
        source: &SourceUrl,
        native_id: &str,
    ) -> Result<Box<dyn NativeSession>, DriverError>;

    fn project(
        &self,
        source: &SourceUrl,
        spec: ProjectSpec,
    ) -> Result<Option<Box<dyn Project>>, DriverError>;

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
    fn attrs(&self) -> &dyn SessionAttrs;
    fn as_any(&self) -> &dyn std::any::Any;
}

pub trait StoredSession {
    fn native_id(&self) -> &str;
    fn content_format(&self) -> &str;
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Box<dyn Entry>, DriverError>> + '_>;
}

/// Attribute reader for a session. Every method defaults to `None`;
/// a driver overrides only the attributes it can answer.
pub trait SessionAttrs {
    fn mtime(&self) -> Option<SystemTime> {
        None
    }
    fn size(&self) -> Option<u64> {
        None
    }
    fn is_empty(&self) -> Option<bool> {
        None
    }
    fn project_name(&self) -> Option<&str> {
        None
    }
    fn project_path(&self) -> Option<&Path> {
        None
    }
    fn title(&self) -> Option<&str> {
        None
    }
    fn model(&self) -> Option<&str> {
        None
    }
    fn message_count(&self) -> Option<u64> {
        None
    }
}

pub enum ProjectSpec {
    Path(PathBuf),
    Name(String),
}

pub trait Project {
    fn name(&self) -> &str;
    fn path(&self) -> Option<&Path>;
    fn is_for(&self, session: &dyn NativeSession) -> bool;
}

pub trait Entry {
    fn line(&self) -> u32;
    fn uuid(&self) -> Option<&str>;
    fn timestamp(&self) -> Option<SystemTime>;
    fn raw(&self) -> Cow<'_, str>;
    fn type_(&self) -> &str;
    fn subtype(&self) -> Option<&str>;
    fn to_message(&self) -> Option<&dyn Message>;
}

pub trait Message {
    fn line(&self) -> u32;
    fn uuid(&self) -> Option<&str>;
    fn type_(&self) -> &str;
    fn subtype(&self) -> Option<&str>;
    fn text(&self) -> Cow<'_, str>;
    fn timestamp(&self) -> Option<SystemTime>;
    fn attachments(&self) -> Option<Cow<'_, str>>;
    fn ide_tags(&self) -> Option<Cow<'_, str>>;
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

/// A parsed `scheme:body` source URL. Fragment and query are not
/// modeled; drivers that need them add them separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUrl {
    scheme: String,
    body: String,
}

impl SourceUrl {
    pub fn new(scheme: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            body: body.into(),
        }
    }

    pub fn parse(s: &str) -> Result<Self, SourceUrlError> {
        let (scheme, body) = s
            .split_once(':')
            .ok_or_else(|| SourceUrlError::NoScheme(s.to_string()))?;
        if scheme.is_empty() {
            return Err(SourceUrlError::EmptyScheme(s.to_string()));
        }
        Ok(Self {
            scheme: scheme.to_string(),
            body: body.to_string(),
        })
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Display for SourceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.scheme, self.body)
    }
}

#[derive(Debug)]
pub enum SourceUrlError {
    NoScheme(String),
    EmptyScheme(String),
}

impl fmt::Display for SourceUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceUrlError::NoScheme(s) => write!(f, "missing scheme in source URL: {s}"),
            SourceUrlError::EmptyScheme(s) => write!(f, "empty scheme in source URL: {s}"),
        }
    }
}

impl std::error::Error for SourceUrlError {}

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
    fn source_url_parses_scheme_and_body() {
        let u = SourceUrl::parse("claude:/tmp/foo").unwrap();
        assert_eq!(u.scheme(), "claude");
        assert_eq!(u.body(), "/tmp/foo");
    }

    #[test]
    fn source_url_parses_empty_body() {
        let u = SourceUrl::parse("claude:").unwrap();
        assert_eq!(u.scheme(), "claude");
        assert_eq!(u.body(), "");
    }

    #[test]
    fn source_url_missing_scheme_rejected() {
        assert!(matches!(
            SourceUrl::parse("no-colon-here"),
            Err(SourceUrlError::NoScheme(_)),
        ));
    }

    #[test]
    fn source_url_empty_scheme_rejected() {
        assert!(matches!(
            SourceUrl::parse(":body"),
            Err(SourceUrlError::EmptyScheme(_)),
        ));
    }

    #[test]
    fn source_url_display_roundtrips() {
        let u = SourceUrl::new("claude", "/tmp/foo");
        assert_eq!(u.to_string(), "claude:/tmp/foo");
    }
}
