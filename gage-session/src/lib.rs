//! Session driver interface.
//!
//! A driver opens a [`Source`] handle over one container of native
//! sessions (Claude Code on disk, a future OpenCode source, a
//! database). The handle serves the source's tables, id lookup, and
//! project naming, and may cache whatever it reads. A driver writes a
//! native session's bytes into the store through a [`ContentSink`]
//! and reads a stored session back through a [`ContentSource`]. A
//! driver may also run an agent on its harness; see [`agent`].

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use datafusion::datasource::TableProvider;

pub mod agent;
pub mod filter;
pub mod status;
pub mod system_col;
pub mod tool_use;

pub use agent::{
    AgentEvent, AgentMcp, AgentSession, AgentSpec, SystemPrompt, ToolUseEntries, TurnResult,
};
pub use status::{AuthInfo, DriverStatus, Field, HarnessInfo, InstallInfo};
pub use system_col::{SYSTEM_META_KEY, is_system, system};
pub use tool_use::{ToolUse, ToolUseBatch, tool_use_batch, tool_use_schema};

/// What a driver opens and how it reads it. `new` names the source as
/// the user gave it: empty for the driver's default location, a body
/// under one of the driver's schemes, or a scheme-less value the
/// driver interprets as it sees fit. The runtime's own in-flight agent
/// sessions are left out unless `show_agents` is set; the live scan
/// view is the one reader that wants them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOptions {
    url: String,
    show_agents: bool,
}

impl SourceOptions {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            show_agents: false,
        }
    }

    pub fn show_agents(&mut self, show: bool) -> &mut Self {
        self.show_agents = show;
        self
    }

    /// The source URL as given
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn shows_agents(&self) -> bool {
        self.show_agents
    }
}

pub trait Driver: Send + Sync {
    /// The driver's name, recorded on stored sessions. Not a scheme.
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;

    /// The source URL schemes this driver serves. A registry routes a
    /// source with one of these schemes to the driver.
    fn schemes(&self) -> &'static [&'static str];

    /// Open a handle over the source `options` names, read as `options`
    /// says. The handle is the driver's place to hold per-source state
    /// (caches, parsed registries, an index) for as long as the caller
    /// keeps it.
    fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError>;

    /// Open the source holding the native session `native_source`
    /// names: the container a stored session's `native_source` came
    /// from. The default treats the value as a source URL; a driver
    /// whose native session URLs differ from its container URLs
    /// overrides it.
    fn open_native_source(&self, native_source: &str) -> Result<Box<dyn Source>, DriverError> {
        self.open_source(&SourceOptions::new(native_source))
    }

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

    /// The display form of a model name from this driver's sessions.
    /// The default shows the name as stored.
    fn format_model(&self, model: &str) -> String {
        model.to_string()
    }

    /// The display form of a project name from this driver's sessions,
    /// shortened to at most `max_chars` characters. The default shows
    /// the name as stored.
    fn format_project(&self, name: &str, _max_chars: usize) -> String {
        name.to_string()
    }

    /// Start an agent on this driver's harness and return the handle
    /// that drives it. The default is that the driver does not start
    /// agents.
    fn start_agent(&self, _spec: AgentSpec) -> Result<Box<dyn AgentSession>, DriverError> {
        Err(DriverError::Other(format!(
            "driver {} does not start agents",
            self.name()
        )))
    }

    /// Verify this driver can serve a scan's support functions, today
    /// starting agents. A scan checks this before it writes anything.
    /// The error is the user-facing reason. The default is ready.
    fn scan_ready(&self) -> Result<(), DriverError> {
        Ok(())
    }

    /// Start an interactive session in the harness that resolves
    /// `issue_ids`, every pending and open issue when empty, with
    /// Gage's tools available to it. The driver verifies its own
    /// preconditions before launching. `model` is as the user gave it;
    /// `None` is the harness's configured model. `extra_args` pass to
    /// the harness verbatim. The default is that the driver does not
    /// start resolve sessions.
    fn start_resolve_session(
        &self,
        _issue_ids: &[String],
        _model: Option<&str>,
        _extra_args: &[String],
    ) -> Result<Box<dyn ResolveSession>, DriverError> {
        Err(DriverError::Other(format!(
            "driver {} does not start resolve sessions",
            self.name()
        )))
    }

    /// What this driver reports about itself. The default reports
    /// every field unsupported.
    fn status(&self) -> Result<DriverStatus, DriverError> {
        Ok(DriverStatus::default())
    }

    /// Install Gage into this driver's harness. `mcp_cmd[0]` is the
    /// Gage executable path; `mcp_cmd[1..]` are the arguments the
    /// harness should launch it with. Progress is reported through
    /// `ui`. The default is that the driver does not install Gage.
    fn system_init(
        &self,
        _mcp_cmd: &[&str],
        _ui: &mut dyn InstallUi,
    ) -> Result<InstallReport, DriverError> {
        Err(DriverError::Other(format!(
            "driver {} does not install Gage",
            self.name()
        )))
    }

    /// Remove Gage from this driver's harness. The default is that
    /// the driver does not install Gage.
    fn system_remove(&self, _ui: &mut dyn InstallUi) -> Result<(), DriverError> {
        Err(DriverError::Other(format!(
            "driver {} does not install Gage",
            self.name()
        )))
    }
}

/// A running interactive resolve session. The session owns the
/// terminal; the caller waits for it to end.
pub trait ResolveSession: Send {
    /// Block until the session ends. Returns the exit code, or -1
    /// when the process was signaled.
    fn wait(&mut self) -> std::io::Result<i32>;

    /// Terminate the session.
    fn kill(&mut self) -> std::io::Result<()>;
}

/// Progress sink for an install or uninstall. The CLI implements it
/// with cliclack spinners; a non-interactive caller may implement it
/// as a no-op or a log line.
pub trait InstallUi {
    /// Begin a step named by `label`.
    fn step(&mut self, label: &str);
    /// End the step most recently started with [`step`](Self::step).
    fn step_done(&mut self);
    /// Record a non-fatal warning.
    fn warn(&mut self, message: &str);
}

/// Result of [`Driver::system_init`].
#[derive(Debug, Clone)]
pub struct InstallReport {
    /// The name under which the harness now knows Gage.
    pub registered_as: String,
    /// Human-readable per-step notes for a post-install summary.
    pub notes: Vec<String>,
}

/// What one [`Source::reconcile`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceOutcome {
    /// Sessions found in the source
    pub discovered: usize,
    /// Sessions whose state was prepared or refreshed
    pub prepared: usize,
    /// Sessions whose state was dropped because they are gone
    pub removed: usize,
    pub elapsed_ms: u64,
}

/// The standing of a source's own state, from [`Source::maintenance_status`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceStatus {
    /// Sessions found in the source
    pub discovered: usize,
    /// Sessions whose state is prepared
    pub prepared: usize,
    /// Sessions whose state is missing or behind the source
    pub stale: usize,
    /// Where the state lives, when it is on disk
    pub state_dir: Option<PathBuf>,
    pub state_bytes: u64,
    /// When [`Source::reconcile`] last ran, in epoch milliseconds
    pub last_reconcile_ms: Option<i64>,
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

    /// Prepare the newest `limit` sessions for a listing read, so a
    /// later `session` table query over them returns without parsing
    /// session content. `on_progress(done, total)` reports the
    /// sessions that need preparing: once with `done == 0`, then after
    /// each one. The default prepares nothing, for drivers whose
    /// listing rows are cheap to read.
    fn warm_summaries(
        &self,
        _limit: usize,
        _on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<(), DriverError> {
        Ok(())
    }

    /// Bring this source's own state up to date with the sessions it
    /// holds, so later reads return without parsing: prepare what is
    /// missing or stale, drop what is gone. `rebuild` discards the
    /// state first. `on_progress(done, total)` reports as
    /// [`warm_summaries`](Self::warm_summaries) does. The default
    /// maintains nothing.
    fn reconcile(
        &self,
        _rebuild: bool,
        _on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<MaintenanceOutcome, DriverError> {
        Ok(MaintenanceOutcome::default())
    }

    /// What [`reconcile`](Self::reconcile) would find. The default
    /// reports nothing to maintain.
    fn maintenance_status(&self) -> Result<MaintenanceStatus, DriverError> {
        Ok(MaintenanceStatus::default())
    }

    fn open_native(&self, native_id: &str) -> Result<Box<dyn NativeSession>, DriverError>;

    /// Remove a native session and any content stored alongside it.
    /// An unknown id is an error.
    fn delete_native(&self, native_id: &str) -> Result<(), DriverError>;

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
    pub tool_use: Arc<dyn TableProvider>,
}

pub trait NativeSession {
    /// The id the harness gave the session
    fn id(&self) -> &str;
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
    /// The session's rows in `entry` table shape, in line order. The
    /// driver deserializes its opaque bytes here; the core builds the
    /// `entry` and `message` tables from the result.
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Entry, DriverError>> + '_>;
    /// The session's tool invocations in `tool_use` table shape, in
    /// call-line order. The core builds the `tool_use` table from the
    /// result.
    fn tool_uses(&mut self) -> Box<dyn Iterator<Item = Result<ToolUse, DriverError>> + '_>;
}

/// The attributes of a native session. An implementation decides how
/// and when it reads them; for the optional ones, `None` means the
/// harness has no such fact, never that the value was not computed.
pub trait SessionAttrs {
    /// When the native artifact was last touched at its source: the
    /// file mtime for a file, the closest equivalent otherwise
    fn native_mtime(&self) -> SystemTime;
    /// The size in bytes of the native artifact
    fn native_size(&self) -> u64;
    /// The session has no entry with content
    fn is_empty(&self) -> bool;
    fn project_name(&self) -> Option<&str>;
    fn title(&self) -> Option<&str>;
    fn model(&self) -> Option<&str>;
    fn message_count(&self) -> Option<u64>;
    /// The number of lines in the native content, when the driver's
    /// content is line-structured. This is the last line number the
    /// `entry` table can report for the session.
    fn line_count(&self) -> Option<u64>;
}

/// One normalized row of a stored session, in `entry` table shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based line in the session's native content
    pub line: u32,
    pub uuid: Option<String>,
    /// The entry type, as the harness names it. Every entry has one;
    /// a native row without a type is not an entry.
    pub entry_type: String,
    pub subtype: Option<String>,
    /// Epoch milliseconds, UTC
    pub timestamp_ms: Option<i64>,
    /// The native row as the harness wrote it, when the harness has
    /// such a thing; a bail-out for readers, not a fact every harness
    /// carries
    pub raw: Option<String>,
    /// Present for exactly the rows the `message` table contains
    pub message: Option<Message>,
}

/// The message-derived columns of an entry. Every message has a type
/// and a text, which may be empty; the rest is present when the
/// harness provides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The message type, e.g. `user` or `assistant`
    pub message_type: String,
    pub subtype: Option<String>,
    pub text: String,
    /// JSON array of attachment blocks, each carrying a `ref` of
    /// `[line, content_index]`
    pub attachments: Option<String>,
    pub ide_tags: Option<String>,
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
    fn source_options_hide_agents_unless_asked() {
        let mut options = SourceOptions::new("claude:/tmp/foo");
        assert_eq!(options.url(), "claude:/tmp/foo");
        assert!(!options.shows_agents());
        options.show_agents(true);
        assert!(options.shows_agents());
    }

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
