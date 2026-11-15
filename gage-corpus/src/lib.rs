//! The derived data layer of a session corpus: a source whose
//! sessions are one line-structured file each.
//!
//! A driver describes its corpus through [`Corpus`]: where the session
//! files are and what one file derives to. Everything else here is
//! shared by every such driver. Two artifacts shadow the corpus,
//! maintained by one reconcile pass: a per-session summary cache at
//! `summary/` and a full-text index over message text at `text/`,
//! both inside the corpus directory `<cache root>/driver/<driver
//! name>/<digest>` that `gage_core::cache` lays out. Both are
//! ephemeral: deleting either is a complete reset of it. The
//! DataFusion table providers in [`tables`] read through them and
//! through one in-memory cache of derived sessions.

mod codec;
mod derive;
mod session_reader;
pub mod shell_paths;
mod store;
mod summary_cache;
pub mod tables;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use codec::JsonlCodec;
pub use derive::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_MESSAGE_SUBTYPE, COL_MESSAGE_TYPE, COL_RAW,
    COL_SESSION_ID, COL_SUBTYPE, COL_TEXT, COL_TIMESTAMP, COL_TYPE, COL_UUID, CorpusSessionAttrs,
    DerivedRows, DerivedSession, Fingerprint, derived_schema,
};
pub use gage_index::LockMode;
pub use session_reader::{ReadError, SessionReader};
pub use store::{IndexStore, ReconcileOutcome, Status};

/// One session file of a corpus, as the listing finds it without
/// deriving it.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub src: PathBuf,
    pub mtime: SystemTime,
    pub size: u64,
    /// The session's project name: the `project` column of the
    /// `session` table
    pub project: String,
}

/// What a driver tells the shared layer about its corpus.
pub trait Corpus: Send + Sync + fmt::Debug {
    /// The driver's name: the `driver` column of its sessions, the
    /// scheme of its session URLs, and the name of its cache
    /// directory.
    fn driver(&self) -> &'static str;

    /// The `session_type` column of the corpus's sessions
    fn session_type(&self) -> &'static str;

    /// The content format the driver writes a session of this corpus
    /// to a store in, and whose codec renders `raw`
    fn content_format(&self) -> &'static str;

    /// The generation of the attributes [`Corpus::derive`] produces.
    /// The driver raises it on any change to what the attributes hold
    /// or mean; a summary cache of another generation is discarded.
    fn summary_version(&self) -> u32;

    /// The harness root the corpus is read from. Session URLs are
    /// `<driver>:<root>/<id>`, and the cache directory is keyed by it.
    fn root(&self) -> &Path;

    /// The directory holding the session files, at or under the root
    fn sessions_dir(&self) -> &Path;

    /// Every session file of the corpus, newest first, from a walk
    /// that derives no session. `show_agents` includes the sessions
    /// of Gage's own in-flight agents, which a listing leaves out.
    fn sessions(&self, show_agents: bool) -> io::Result<Vec<SessionInfo>>;

    /// Parse the session file at `path` and derive its rows and
    /// attributes.
    fn derive(&self, id: &str, path: &Path) -> Result<DerivedSession>;

    /// The title the harness records for the session `id` outside its
    /// session file, which takes the place of the derived one. The
    /// default is that the harness records none there.
    fn recorded_title(&self, _id: &str) -> Option<String> {
        None
    }
}

#[derive(Debug)]
pub enum IndexError {
    Io(io::Error),
    Arrow(arrow::error::ArrowError),
    Json(serde_json::Error),
    Text(gage_index::Error),
    /// A session line that does not read or decode. The session is
    /// not derived: a line in a shape the codec does not recognize is
    /// a session the build knows nothing about.
    Read(ReadError),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Io(e) => write!(f, "io error: {e}"),
            IndexError::Arrow(e) => write!(f, "arrow error: {e}"),
            IndexError::Json(e) => write!(f, "json error: {e}"),
            IndexError::Text(e) => write!(f, "{e}"),
            IndexError::Read(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<io::Error> for IndexError {
    fn from(e: io::Error) -> Self {
        IndexError::Io(e)
    }
}

impl From<arrow::error::ArrowError> for IndexError {
    fn from(e: arrow::error::ArrowError) -> Self {
        IndexError::Arrow(e)
    }
}

impl From<serde_json::Error> for IndexError {
    fn from(e: serde_json::Error) -> Self {
        IndexError::Json(e)
    }
}

impl From<gage_index::Error> for IndexError {
    fn from(e: gage_index::Error) -> Self {
        IndexError::Text(e)
    }
}

impl From<ReadError> for IndexError {
    fn from(e: ReadError) -> Self {
        IndexError::Read(e)
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;

#[cfg(test)]
pub(crate) mod testing;
