//! `PiDriver` implements `gage_session::Driver` for Pi sessions
//! stored under a Pi agent directory as
//! `sessions/--<encoded cwd>--/<timestamp>_<id>.jsonl`.
//!
//! Source grammar, with or without the `pi:` scheme:
//! - `` (empty) -- the default location: the sessions directory
//!   `$PI_CODING_AGENT_SESSION_DIR` names, else `sessions` under
//!   `$PI_CODING_AGENT_DIR`, else under `$HOME/.pi/agent`
//! - `<path>` -- an agent directory, whose sessions are under
//!   `<path>/sessions`, or a sessions directory itself
//!
//! Session files are found in the sessions directory and one level
//! below it. A session Pi wrote somewhere else, through its
//! `sessionDir` setting or `--session <path>`, is found only when the
//! source names the directory it is in.
//!
//! A session's own URL is `pi:<absolute root>/<id>`, always with the
//! resolved root, so the same file read through any spelling of its
//! source records the same `native_source`.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{self, Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use gage_core::config::default_cache_root;
use gage_corpus::tables::{EntryTable, MessageTable, SessionTable, ToolUseFileTable, ToolUseTable};
use gage_corpus::{
    CorpusSessionAttrs, Fingerprint, IndexStore, LockMode, SessionInfo, SessionReader,
    decoded_entries, format_project_slug, project_slug, write_derived,
};
use gage_plugin::{MCP_SERVER_NAME, write_plugin};
use gage_session::{
    AgentSession, AgentSpec, ContentSchema, ContentSink, ContentSource, DecodedEntry, Derivation,
    Driver, DriverError, DriverStatus, DriverTables, Entry, EntryBody, EntryCodec, EntryValue,
    Field, HarnessSession, HostContext, InstallReport, InstallUi, MaintenanceOutcome,
    MaintenanceStatus, MessageTextFilter, MessageTextHit, ModelChoices, ModelSizes,
    NativeLookupError, NativeSession, SessionAttrs, SkillSession, Source, SourceOptions,
    StoredSession, SummaryAttrs, ToolUse, WrittenContent, split_scheme,
};
use serde_json::Value;

use crate::body;
use crate::codec::codec;
use crate::content_schema;
use crate::corpus::{PiCorpus, working_dir};
use crate::derive::{SUMMARY_VERSION, derive_entry, derive_session_from};
use crate::harness::{self, HARNESS_NAME};
use crate::home::Location;
use crate::model::{DEFAULT_MODEL, default_sizes, model_choices, resolved_model};
use crate::plugin;
use crate::skill::skill_command;
use crate::tool_use::ToolUseCollector;

pub(crate) const NAME: &str = "pi";
const SCHEMES: &[&str] = &["pi"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const SESSION_TYPE: &str = "pi";
/// `content_format` value returned by `write_native` and expected by
/// `read_stored`. The name identifies the byte layout under
/// `files.d/`; the trailing digit is its version. It is not the
/// version of Pi's session format, which the file's header carries.
pub(crate) const CONTENT_FORMAT: &str = "pi-jsonl 1";
/// The path of the session file in a stored session's content
const CONTENT_PATH: &str = "session.jsonl";
/// The summary attribute holding the version of Pi's session format
/// a session file is in
const FORMAT_VERSION_ATTR: &str = "format_version";
/// The variable in which Pi names the session to the shell commands
/// the session runs
const SESSION_ID_VAR: &str = "PI_SESSION_ID";

pub struct PiDriver {
    /// The model each size resolves to
    models: ModelSizes,
}

impl PiDriver {
    pub fn new() -> Self {
        Self::with_models(&ModelSizes::default())
    }

    /// A driver whose sizes resolve to `overrides` where it sets a
    /// model, and to the defaults elsewhere.
    pub fn with_models(overrides: &ModelSizes) -> Self {
        Self {
            models: default_sizes().overridden_by(overrides),
        }
    }

    /// The model an agent runs on when its spec names none. Pi signs
    /// in to each model provider on its own, so this is the model the
    /// driver's sign-in checks ask about.
    fn default_model(&self) -> String {
        resolved_model(Some(DEFAULT_MODEL), &self.models)
    }
}

impl Default for PiDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for PiDriver {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> &'static str {
        VERSION
    }

    fn schemes(&self) -> &'static [&'static str] {
        SCHEMES
    }

    fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
        Ok(Box::new(PiSource::open(options)?))
    }

    /// A session URL is `pi:<root>/<id>`; its source is `pi:<root>`. A
    /// bare id is a session in the default location, `pi:`.
    fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
        let body = match split_scheme(session_url) {
            Some((scheme, _)) if scheme != NAME => {
                return Err(DriverError::Other(format!(
                    "{session_url}: not a {NAME} session URL"
                )));
            }
            Some((_, body)) => body,
            None => session_url,
        };
        let (root, id) = body.rsplit_once('/').unwrap_or(("", body));
        if id.is_empty() {
            return Err(DriverError::Other(format!(
                "{session_url}: session URL has no session id"
            )));
        }
        Ok((format!("{NAME}:{root}"), id.to_string()))
    }

    fn codec(&self, content_format: &str) -> Result<Arc<dyn EntryCodec>, DriverError> {
        if content_format != CONTENT_FORMAT {
            return Err(DriverError::Other(format!(
                "content format {content_format:?} has no codec in this build; \
                 this build reads {CONTENT_FORMAT:?} (upgrade gage)"
            )));
        }
        Ok(codec())
    }

    fn entry_body(&self, content_format: &str, entry: &EntryValue) -> EntryBody {
        if content_format != CONTENT_FORMAT {
            return EntryBody::default();
        }
        body::entry_body(entry)
    }

    fn format_project(&self, name: &str, max_chars: usize) -> String {
        format_project_slug(name, max_chars)
    }

    fn content_schemas(&self) -> Vec<ContentSchema> {
        content_schema::content_schemas()
    }

    /// Pi names the session in the environment of each shell command
    /// the session runs. It names it nowhere for an MCP server: the
    /// server's environment is Pi's own, and a tool call carries
    /// nothing of the session. The session is one in the default
    /// location.
    fn current_session(&self, host: &HostContext<'_>) -> Result<Option<String>, DriverError> {
        let Some(id) = host.var(SESSION_ID_VAR).filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        let root = resolve_location("")?.root;
        Ok(Some(format!("{NAME}:{}/{id}", root.display())))
    }

    fn model_sizes(&self) -> ModelSizes {
        self.models.clone()
    }

    fn model_choices(&self) -> ModelChoices {
        model_choices()
    }

    /// Spawn a `pi --mode json` child; see [`crate::agent`]. Requires
    /// a tokio runtime.
    fn start_agent(&self, spec: AgentSpec) -> Result<Box<dyn AgentSession>, DriverError> {
        Ok(Box::new(crate::agent::start(spec, &self.models)?))
    }

    /// A scan's agents need `pi` and working credentials for the
    /// provider of the model they run on. The model asked about is
    /// the default one; an agent whose spec names a model of another
    /// provider is refused by Pi when it starts.
    fn scan_ready(&self) -> Result<(), DriverError> {
        harness::check_auth(&self.default_model()).map_err(DriverError::Other)
    }

    /// Start the skill session. Gage must be installed in Pi at this
    /// build's version, and a session that names a model needs
    /// working credentials for it. Pi has no sandbox: a batch
    /// session's `gage` commands run as the user.
    fn start_skill_session(
        &self,
        session: &SkillSession<'_>,
    ) -> Result<Box<dyn HarnessSession>, DriverError> {
        let pi = harness::require_pi().map_err(DriverError::Other)?;
        plugin::check_plugin().map_err(DriverError::Other)?;
        if let Some(model) = session.model {
            harness::check_auth(&resolved_model(Some(model), &self.models))
                .map_err(DriverError::Other)?;
        }
        let child = skill_command(&pi, session, &self.models)
            .spawn()
            .map_err(DriverError::Io)?;
        Ok(Box::new(PiHarnessSession(child)))
    }

    fn harness_name(&self) -> &'static str {
        HARNESS_NAME
    }

    fn harness_path(&self) -> Option<PathBuf> {
        harness::find_pi()
    }

    /// The CLI, its sign-in for the default model's provider, and
    /// Gage's install. A CLI that is not installed is absent, and
    /// nothing more is asked of it.
    fn status(&self) -> Result<DriverStatus, DriverError> {
        let harness = harness::harness_status();
        let (auth, install) = match &harness {
            Field::Ok(info) => (
                harness::auth_status(&info.path, &self.default_model()),
                plugin::install_status(),
            ),
            _ => (Field::Unsupported, Field::Unsupported),
        };
        Ok(DriverStatus {
            harness,
            auth,
            install,
        })
    }

    /// Render the plugin as a Pi package, have Pi load it from there,
    /// and register the MCP server. Pi loads a local package from
    /// where it is and replaces a server registration of the same
    /// name, so an install over an earlier one removes nothing first.
    fn system_init(
        &self,
        mcp_cmd: &[&str],
        ui: &mut dyn InstallUi,
    ) -> Result<InstallReport, DriverError> {
        if mcp_cmd.is_empty() {
            return Err(DriverError::Other(
                "system_init: mcp_cmd must name an executable".into(),
            ));
        }
        let pi = harness::require_pi().map_err(DriverError::Other)?;
        let dir = plugin::plugin_dir();
        let mut notes = Vec::new();

        ui.step("Writing Pi package files");
        write_plugin(&dir, &plugin::target()).map_err(DriverError::Io)?;
        notes.push(format!("package materialized at {}", dir.display()));
        ui.step_done();

        ui.step("Installing Pi package");
        plugin::run_strict(&pi, &["install".as_ref(), dir.as_os_str()])?;
        notes.push("package installed".to_string());
        ui.step_done();

        ui.step("Registering Pi MCP server");
        plugin::run_strict(&pi, &plugin::mcp_add_args(mcp_cmd))?;
        notes.push(format!("MCP server '{MCP_SERVER_NAME}' registered"));
        ui.step_done();

        Ok(InstallReport {
            registered_as: MCP_SERVER_NAME.to_string(),
            notes,
        })
    }

    /// Remove the package from Pi's settings, the MCP server's
    /// registration, and the package's files. Each removal Pi refuses
    /// is a warning: before a first install nothing is there to
    /// remove.
    fn system_remove(&self, ui: &mut dyn InstallUi) -> Result<(), DriverError> {
        let pi = harness::require_pi().map_err(DriverError::Other)?;
        let dir = plugin::plugin_dir();
        ui.step("Uninstalling Pi package");
        let commands = [
            vec!["remove".as_ref(), dir.as_os_str()],
            plugin::mcp_remove_args().to_vec(),
        ];
        for args in commands {
            if let Err(said) = plugin::run_lenient(&pi, &args)? {
                ui.warn(&said);
            }
        }
        if let Err(e) = fs::remove_dir_all(&dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(DriverError::Io(e));
        }
        ui.step_done();
        Ok(())
    }

    /// The session file is stored as it is read here, whatever
    /// version of Pi's format it is in: the driver does not migrate. A
    /// change to the file after the read is not included. One read
    /// serves the write and the derive, so the derived attributes and
    /// the size describe the bytes stored. The mtime is the one read
    /// when the session was opened.
    fn write_native(
        &self,
        session: &mut dyn NativeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<WrittenContent, DriverError> {
        let pi = session
            .as_any()
            .downcast_ref::<PiNativeSession>()
            .ok_or_else(|| {
                DriverError::Other("write_native: session is not a PiNativeSession".into())
            })?;
        let info = pi.info.clone();
        let bytes = fs::read(&info.src).map_err(DriverError::Io)?;
        sink.create(CONTENT_PATH)
            .and_then(|mut w| w.write_all(&bytes))
            .map_err(DriverError::Io)?;
        let fingerprint = Fingerprint {
            mtime_ms: millis(info.mtime),
            size: bytes.len() as u64,
        };
        let store = pi.store.clone();
        Ok(WrittenContent::with_derive(
            CONTENT_FORMAT,
            SUMMARY_VERSION,
            move || {
                let derived = derive_session_from(
                    &info.id,
                    SessionReader::new(bytes.as_slice(), codec()),
                    fingerprint,
                    info.project.clone(),
                )
                .map_err(|e| DriverError::Other(format!("{}: {e}", info.src.display())))?;
                // The cache is rebuilt from the session file on any
                // read that misses it, so a failed write costs a later
                // read and is logged rather than raised.
                if let Err(e) = store.put_session_attrs(&info.id, &derived.attrs) {
                    tracing::warn!(session_id = %info.id, "summary cache write failed: {e}");
                }
                Ok(Box::new(derived.attrs) as Box<dyn SessionAttrs>)
            },
        ))
    }

    /// The file in the later version of Pi's format is the later
    /// state, whatever its length: Pi migrates a file of an earlier
    /// version by rewriting it, and the rewrite drops the blank and
    /// unparsable lines, so the later state can be the shorter one.
    /// Within one version a session grows by lines: more lines is a
    /// later state, fewer is an older copy, and the same count with
    /// other bytes is a modification in place.
    fn native_order(&self, native: &SummaryAttrs, stored: &SummaryAttrs) -> Ordering {
        let version = |summary: &SummaryAttrs| {
            summary
                .extra
                .get(FORMAT_VERSION_ATTR)
                .and_then(Value::as_u64)
        };
        if let (Some(native), Some(stored)) = (version(native), version(stored))
            && native != stored
        {
            return native.cmp(&stored);
        }
        match (native.line_count, stored.line_count) {
            (Some(native), Some(stored)) => native.cmp(&stored),
            _ => Ordering::Greater,
        }
    }

    /// A stored session is read under the format this build writes;
    /// any other value names bytes this build cannot interpret.
    fn read_stored(
        &self,
        native_id: String,
        content_format: &str,
        source: Box<dyn ContentSource>,
    ) -> Result<Box<dyn StoredSession>, DriverError> {
        if content_format != CONTENT_FORMAT {
            return Err(DriverError::Other(format!(
                "stored session {native_id} has content format {content_format:?}; \
                 this build reads {CONTENT_FORMAT:?} (upgrade gage)"
            )));
        }
        Ok(Box::new(PiStoredSession {
            native_id,
            content_format: content_format.to_string(),
            source,
        }))
    }
}

/// Handle over one Pi location. Holds the index store for the
/// location's session files.
pub struct PiSource {
    source: String,
    /// The location's root, absolute
    root: PathBuf,
    store: Arc<IndexStore>,
    /// Session id to session, walked once: one walk serves every
    /// lookup of the handle.
    sessions: OnceLock<Result<HashMap<String, SessionInfo>, String>>,
}

impl PiSource {
    /// The session an agent of this location ran as, by its id, for
    /// storing it. `None` when Pi wrote no session file for the id.
    pub(crate) fn agent_session(
        &self,
        session_id: &str,
    ) -> Result<Option<PiNativeSession>, DriverError> {
        if !self.sessions()?.contains_key(session_id) {
            return Ok(None);
        }
        Ok(Some(PiNativeSession {
            source: self.session_url(session_id),
            info: self.session(session_id)?,
            store: Arc::clone(&self.store),
            attrs: OnceCell::new(),
        }))
    }

    fn open(options: &SourceOptions) -> Result<Self, DriverError> {
        let source = options.url();
        let location = resolve_location(source)?;
        let cache_root = options
            .cache_root_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_cache_root);
        let root = location.root.clone();
        let corpus = Arc::new(PiCorpus::new(location));
        let store =
            Arc::new(IndexStore::new(corpus, cache_root).show_agents(options.shows_agents()));
        store.ensure_summary_dir()?;
        Ok(Self {
            source: source.to_string(),
            root,
            store,
            sessions: OnceLock::new(),
        })
    }

    fn sessions(&self) -> Result<&HashMap<String, SessionInfo>, DriverError> {
        self.sessions
            .get_or_init(|| {
                self.store
                    .sessions()
                    .map(|list| list.into_iter().map(|s| (s.id.clone(), s)).collect())
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| DriverError::Other(format!("reading sessions: {e}")))
    }

    /// The session `native_id` with a fresh stat of its file. The stat
    /// is taken per call so a file removed after the walk reports not
    /// found and the mtime reflects the file as it is now.
    fn session(&self, native_id: &str) -> Result<SessionInfo, DriverError> {
        let not_found =
            || DriverError::Other(format!("native session not found: {NAME}:{native_id}"));
        let listed = self.sessions()?.get(native_id).ok_or_else(not_found)?;
        match listed.src.metadata() {
            Ok(meta) => Ok(SessionInfo {
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: meta.len(),
                ..listed.clone()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(not_found()),
            Err(e) => Err(DriverError::Io(e)),
        }
    }
}

impl Source for PiSource {
    fn source(&self) -> &str {
        &self.source
    }

    fn session_url(&self, session_id: &str) -> String {
        format!("{NAME}:{}/{session_id}", self.root.display())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn tables(&self) -> Result<DriverTables, DriverError> {
        Ok(DriverTables {
            session: Arc::new(SessionTable::new(Arc::clone(&self.store))),
            message: Arc::new(MessageTable::new(Arc::clone(&self.store))),
            entry: Arc::new(EntryTable::new(Arc::clone(&self.store))),
            tool_use: Arc::new(ToolUseTable::new(Arc::clone(&self.store))),
            tool_use_file: Arc::new(ToolUseFileTable::new(Arc::clone(&self.store))),
        })
    }

    fn warm_summaries(&self, limit: usize) -> Result<(), DriverError> {
        Ok(self.store.warm(limit)?)
    }

    /// The reconcile pass over the summary cache: every session the
    /// cache does not hold at its current fingerprint is derived, and
    /// the cache of every session gone from disk is removed.
    fn reconcile(&self, rebuild: bool) -> Result<MaintenanceOutcome, DriverError> {
        let outcome = if rebuild {
            self.store.rebuild()
        } else {
            self.store.reconcile(LockMode::Wait)
        }
        .map_err(|e| DriverError::Other(e.to_string()))?;
        Ok(MaintenanceOutcome {
            discovered: outcome.discovered,
            prepared: outcome.indexed,
            removed: outcome.removed,
            elapsed_ms: outcome.elapsed_ms,
        })
    }

    fn maintenance_status(&self) -> Result<MaintenanceStatus, DriverError> {
        let status = self
            .store
            .status()
            .map_err(|e| DriverError::Other(e.to_string()))?;
        Ok(MaintenanceStatus {
            discovered: status.discovered,
            prepared: status.indexed,
            stale: status.dirty,
            state_dir: Some(status.summary_dir),
            state_bytes: status.cache_bytes,
            text_dir: Some(status.text_dir),
            text_bytes: status.text_bytes,
            last_reconcile_ms: status.last_reconcile_ms,
        })
    }

    /// Search the text index, after a reconcile that yields to a pass
    /// already running: on contention the committed index is searched
    /// as it stands, at most one pass behind the corpus.
    fn message_text(
        &self,
        query: &str,
        filter: &MessageTextFilter,
        snippet_chars: usize,
        limit: usize,
    ) -> Result<Vec<MessageTextHit>, DriverError> {
        self.store
            .reconcile(LockMode::Try)
            .map_err(|e| DriverError::Other(e.to_string()))?;
        self.store
            .search(query, filter, limit, snippet_chars)
            .map_err(|e| DriverError::Other(e.to_string()))
    }

    fn find_native(&self, prefix: &str) -> Result<String, NativeLookupError> {
        let sessions = self.sessions().map_err(NativeLookupError::Driver)?;
        let mut matches: Vec<String> = sessions
            .keys()
            .filter(|id| id.starts_with(prefix))
            .cloned()
            .collect();
        match matches.len() {
            0 => Err(NativeLookupError::NoMatch(prefix.to_string())),
            1 => Ok(matches.pop().unwrap()),
            _ => {
                matches.sort();
                Err(NativeLookupError::TooManyMatches {
                    prefix: prefix.to_string(),
                    candidates: matches,
                })
            }
        }
    }

    fn open_native(&self, native_id: &str) -> Result<Box<dyn NativeSession>, DriverError> {
        Ok(Box::new(PiNativeSession {
            source: self.session_url(native_id),
            info: self.session(native_id)?,
            store: Arc::clone(&self.store),
            attrs: OnceCell::new(),
        }))
    }

    fn project_name(&self, path: &Path) -> Result<String, DriverError> {
        let canonical = match fs::canonicalize(path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
            Err(e) => return Err(DriverError::Io(e)),
        };
        Ok(project_slug(&canonical))
    }

    /// The working directory a session of the project started in, as
    /// its header records it.
    fn project_path(&self, name: &str) -> Result<Option<PathBuf>, DriverError> {
        let Some(session) = self.sessions()?.values().find(|s| s.project == name) else {
            return Ok(None);
        };
        Ok(working_dir(&session.src)?)
    }

    fn close(self: Box<Self>) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Resolve the location a source value names. A leading `pi:` scheme
/// is stripped; any other scheme is an error. An empty body is the
/// default location. A non-empty body is a directory (`~` expanded).
/// The root is absolute, so a session URL spelled from it is location
/// independent.
fn resolve_location(source: &str) -> Result<Location, DriverError> {
    let body = match split_scheme(source) {
        Some((scheme, body)) if SCHEMES.contains(&scheme) => body,
        Some((scheme, _)) => {
            return Err(DriverError::Other(format!(
                "unsupported source scheme {scheme:?}: {source}"
            )));
        }
        None => source,
    };
    let location = if body.is_empty() {
        Location::default_location().ok_or_else(|| {
            DriverError::Other(
                "PI_CODING_AGENT_SESSION_DIR, PI_CODING_AGENT_DIR, or HOME must be set".into(),
            )
        })?
    } else {
        Location::with_dir(&path::absolute(expand_tilde(body)).map_err(DriverError::Io)?)
    };
    Ok(Location {
        root: path::absolute(&location.root).map_err(DriverError::Io)?,
        sessions_dir: path::absolute(&location.sessions_dir).map_err(DriverError::Io)?,
    })
}

fn expand_tilde(s: &str) -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from);
    match (s.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        (None, Some(home)) if s == "~" => home,
        _ => PathBuf::from(s),
    }
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// A native Pi session. Its attributes come from the source's summary
/// cache when the cache entry is current for the file's mtime, else
/// from one parse of the file, which then refreshes the cache.
pub struct PiNativeSession {
    /// `pi:<absolute root>/<session id>`
    source: String,
    /// The session file as the caller last read it from the
    /// filesystem, before the session was opened
    info: SessionInfo,
    store: Arc<IndexStore>,
    /// The attributes, read on first use
    attrs: OnceCell<CorpusSessionAttrs>,
}

impl PiNativeSession {
    /// The session file
    pub(crate) fn path(&self) -> &Path {
        &self.info.src
    }

    /// Record `project` as the session's project in place of the slug
    /// of the directory it started in.
    pub(crate) fn set_project(&mut self, project: String) {
        if let Some(attrs) = self.attrs.get_mut() {
            attrs.project = project.clone();
        }
        self.info.project = project;
    }
}

/// The skill session's `pi` process.
struct PiHarnessSession(Child);

impl HarnessSession for PiHarnessSession {
    fn wait(&mut self) -> io::Result<i32> {
        Ok(self.0.wait()?.code().unwrap_or(-1))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }
}

impl NativeSession for PiNativeSession {
    fn id(&self) -> &str {
        &self.info.id
    }

    fn session_type(&self) -> &str {
        SESSION_TYPE
    }

    fn source(&self) -> &str {
        &self.source
    }

    fn attrs(&self) -> &dyn SessionAttrs {
        self.attrs.get_or_init(|| self.store.attrs_of(&self.info))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct PiStoredSession {
    native_id: String,
    content_format: String,
    source: Box<dyn ContentSource>,
}

impl PiStoredSession {
    fn reader(&self) -> io::Result<SessionReader<Box<dyn Read + Send>>> {
        Ok(SessionReader::new(self.source.open(CONTENT_PATH)?, codec()))
    }
}

impl StoredSession for PiStoredSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }

    fn content_format(&self) -> &str {
        &self.content_format
    }

    /// One normalized row per parsed line of the session file, through
    /// the same derivation the native tables use. A blank line and a
    /// line without a type consume a line number and yield no row; a
    /// line that does not read or decode is an error naming the line.
    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Entry, DriverError>> + '_> {
        let reader = match self.reader() {
            Ok(reader) => reader,
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        let native_id = self.native_id.clone();
        Box::new(reader.filter_map(move |item| match item {
            Ok((line_num, raw, decoded)) => {
                derive_entry(&native_id, line_num, &Value::from(decoded), raw).map(Ok)
            }
            Err(e) => Some(Err(DriverError::from(e))),
        }))
    }

    /// The tool calls of the session, through the same collector the
    /// native table uses. A call's result is known only once a later
    /// line is read, so the rows are read before the first is yielded;
    /// read failures follow the rows. Relative paths resolve against
    /// the working directory the header names.
    fn tool_uses(&mut self) -> Box<dyn Iterator<Item = Result<ToolUse, DriverError>> + '_> {
        let reader = match self.reader() {
            Ok(reader) => reader,
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        let mut collector = ToolUseCollector::new();
        let mut failures = Vec::new();
        for item in reader {
            match item {
                Ok((line_num, _, decoded)) => collector.push(line_num, &Value::from(decoded)),
                Err(e) => failures.push(Err(DriverError::from(e))),
            }
        }
        Box::new(collector.finish().into_iter().map(Ok).chain(failures))
    }

    fn entry_values(&mut self) -> Box<dyn Iterator<Item = Result<DecodedEntry, DriverError>> + '_> {
        decoded_entries(&*self.source, CONTENT_PATH, codec())
    }

    fn write_derived(
        &mut self,
        derivation: &Derivation,
        sink: &mut dyn ContentSink,
    ) -> Result<Vec<u32>, DriverError> {
        write_derived(&*self.source, CONTENT_PATH, &*codec(), derivation, sink)
    }

    /// The session's attributes through the same derivation a native
    /// session gets. The project is the store's to keep: the stored
    /// content is read without the listing that names it.
    fn attrs(&mut self) -> Result<Box<dyn SessionAttrs>, DriverError> {
        let mut bytes = Vec::new();
        self.source.open(CONTENT_PATH)?.read_to_end(&mut bytes)?;
        let fingerprint = Fingerprint {
            mtime_ms: 0,
            size: bytes.len() as u64,
        };
        let derived = derive_session_from(
            &self.native_id,
            SessionReader::new(bytes.as_slice(), codec()),
            fingerprint,
            String::new(),
        )
        .map_err(|e| DriverError::Other(format!("stored session {}: {e}", self.native_id)))?;
        Ok(Box::new(derived.attrs))
    }

    fn summary_version(&self) -> u32 {
        SUMMARY_VERSION
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use gage_session::{BodyBlock, FileType, TypedSet};

    use super::*;
    use crate::testing::{
        GENERATED, V1_EXTENDED, V1_EXTENDED_ID, V1_PLAIN, V3_BRANCH, V3_HEADER_V1_ENTRIES, V3_TORN,
        generated, generated_agent_dir, lines, seed,
    };

    fn options_for(dir: &Path, cache: &Path) -> SourceOptions {
        let mut options = SourceOptions::new(&format!("pi:{}", dir.display()));
        options.cache_root(cache.join("cache"));
        options
    }

    /// A source over the sessions directory `dir`, with its cache
    /// beside it
    fn source_for(dir: &Path) -> Box<dyn Source> {
        PiDriver::new().open_source(&options_for(dir, dir)).unwrap()
    }

    /// A session file as stored content
    struct Stored(String);

    impl ContentSource for Stored {
        fn paths(&self) -> io::Result<Vec<String>> {
            Ok(vec![CONTENT_PATH.to_string()])
        }
        fn open(&self, _path: &str) -> io::Result<Box<dyn Read + Send>> {
            Ok(Box::new(io::Cursor::new(self.0.clone().into_bytes())))
        }
    }

    #[derive(Default)]
    struct MemorySink(BTreeMap<String, Vec<u8>>);

    impl ContentSink for MemorySink {
        fn create<'a>(&'a mut self, path: &str) -> io::Result<Box<dyn Write + 'a>> {
            Ok(Box::new(self.0.entry(path.to_string()).or_default()))
        }
    }

    #[test]
    fn an_agent_directory_lists_the_sessions_pi_filed_under_it() {
        let cache = tempfile::tempdir().unwrap();
        let agent_dir = generated_agent_dir();
        let source = PiDriver::new()
            .open_source(&options_for(&agent_dir, cache.path()))
            .unwrap();
        for name in GENERATED {
            let (id, _) = generated(name);
            assert_eq!(source.find_native(&id).unwrap(), id, "{name}");
        }
        let (run, _) = generated("run");
        let session = source.open_native(&run).unwrap();
        assert_eq!(
            session.source(),
            format!("pi:{}/{run}", agent_dir.display())
        );
        assert_eq!(session.session_type(), "pi");
        assert_eq!(
            session.attrs().project_name(),
            Some("-tmp-gage-pi-fixtures-project"),
            "the slug of the working directory the header names"
        );
        assert_eq!(
            source
                .project_path("-tmp-gage-pi-fixtures-other-project")
                .unwrap(),
            Some(PathBuf::from("/tmp/gage-pi-fixtures/other-project"))
        );
        assert_eq!(source.project_path("-no-such").unwrap(), None);
        assert_eq!(
            source.project_name(Path::new("/w/my.proj")).unwrap(),
            "-w-my-proj"
        );
    }

    #[test]
    fn a_sessions_directory_is_a_source_with_its_files_flat_or_filed() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), "filed", V1_PLAIN);
        let flat = dir.path().join("2025-06-01T10-00-00-000Z_flat.jsonl");
        fs::write(&flat, V3_BRANCH).unwrap();

        let source = source_for(dir.path());
        assert_eq!(source.find_native("fil").unwrap(), "filed");
        assert_eq!(source.find_native("flat").unwrap(), "flat");
        assert!(matches!(
            source.find_native("f"),
            Err(NativeLookupError::TooManyMatches { .. })
        ));
        let session = source.open_native("flat").unwrap();
        assert_eq!(session.attrs().project_name(), Some("-w-tree"));
        assert_eq!(session.attrs().title(), Some("Pick a name"));
    }

    /// A session one of Gage's agent runs wrote is the run's: a
    /// listing leaves it out, and the run reads it by its id to store
    /// it, under the project it was given.
    #[test]
    fn an_agent_runs_session_is_listed_only_with_agents_shown() {
        let dir = tempfile::tempdir().unwrap();
        let run_cwd = gage_core::config::gage_home()
            .join("tmp")
            .join("run-1")
            .join("cwd");
        let agent = V3_BRANCH.replace("/w/tree", run_cwd.to_str().unwrap());
        assert_ne!(agent, V3_BRANCH);
        seed(dir.path(), "agent", &agent);
        seed(dir.path(), "users", V3_BRANCH);

        let source = source_for(dir.path());
        assert_eq!(source.find_native("").unwrap(), "users");

        let mut options = options_for(dir.path(), dir.path());
        options.show_agents(true);
        let source = PiDriver::new().open_source(&options).unwrap();
        assert_eq!(source.find_native("age").unwrap(), "agent");
        let pi = source.as_any().downcast_ref::<PiSource>().unwrap();
        assert!(pi.agent_session("missing").unwrap().is_none());
        let mut session = pi.agent_session("agent").unwrap().unwrap();
        assert_eq!(session.id(), "agent");
        assert!(
            session
                .path()
                .ends_with("2025-06-01T10-00-00-000Z_agent.jsonl")
        );
        session.set_project("given".to_string());
        assert_eq!(session.attrs().project_name(), Some("given"));
    }

    #[test]
    fn the_current_session_is_the_one_the_environment_names() {
        let driver = PiDriver::new();
        let none = driver
            .current_session(&HostContext::with_vars(&[]))
            .unwrap();
        assert_eq!(none, None);

        let vars = [(SESSION_ID_VAR, "s-env")];
        let url = driver
            .current_session(&HostContext::with_vars(&vars))
            .unwrap()
            .unwrap();
        let (source, id) = driver.split_session_url(&url).unwrap();
        assert_eq!(id, "s-env");
        assert_eq!(
            source,
            format!("pi:{}", resolve_location("").unwrap().root.display())
        );

        // A tool call names no session, whatever it carries
        let meta = serde_json::json!({"progressToken": 1});
        let host = HostContext::with_vars(&[]).serving_call(meta.as_object().unwrap());
        assert_eq!(driver.current_session(&host).unwrap(), None);
    }

    #[test]
    fn model_sizes_are_the_defaults_with_the_configured_ones_over_them() {
        let defaults = PiDriver::new().model_sizes();
        assert_eq!(
            defaults.small.as_deref(),
            Some("anthropic/claude-haiku-4-5")
        );
        assert_eq!(
            defaults.medium.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
        let overrides = ModelSizes {
            medium: Some("openai/gpt-6.1-terra".to_string()),
            ..ModelSizes::default()
        };
        let driver = PiDriver::with_models(&overrides);
        assert_eq!(
            driver.model_sizes().medium.as_deref(),
            Some("openai/gpt-6.1-terra")
        );
        assert_eq!(driver.model_sizes().small, defaults.small);
        assert_eq!(driver.default_model(), "openai/gpt-6.1-terra");
    }

    #[test]
    fn a_file_with_no_header_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let headless: String = V3_BRANCH
            .lines()
            .skip(1)
            .map(|l| format!("{l}\n"))
            .collect();
        seed(dir.path(), "headless", &headless);
        seed(dir.path(), "empty", "");
        seed(dir.path(), "real", V3_BRANCH);
        let source = source_for(dir.path());
        assert_eq!(source.find_native("").unwrap(), "real");
    }

    #[test]
    fn the_newest_of_two_files_with_one_id_is_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let older = seed(dir.path(), "named", V1_PLAIN);
        let other = dir.path().join("--w-other--");
        fs::create_dir_all(&other).unwrap();
        let newer = other.join("2025-06-02T10-00-00-000Z_named.jsonl");
        fs::write(&newer, V3_BRANCH).unwrap();
        let earlier = SystemTime::now() - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(earlier)
            .unwrap();

        let source = source_for(dir.path());
        let session = source.open_native("named").unwrap();
        assert_eq!(session.attrs().project_name(), Some("-w-tree"));
    }

    #[test]
    fn a_session_url_splits_into_its_source_and_its_id() {
        let driver = PiDriver::new();
        let (source, id) = driver.split_session_url("pi:/a/agent/my_run.v2").unwrap();
        assert_eq!((source.as_str(), id.as_str()), ("pi:/a/agent", "my_run.v2"));
        let (source, id) = driver.split_session_url("abc").unwrap();
        assert_eq!((source.as_str(), id.as_str()), ("pi:", "abc"));
        assert!(driver.split_session_url("codex:/x/y").is_err());
        assert!(driver.split_session_url("pi:/a/agent/").is_err());
        assert_eq!(driver.format_project("-home-u-proj", 25), "proj");
    }

    #[test]
    fn write_native_stores_the_file_as_it_is_and_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed(dir.path(), V1_EXTENDED_ID, V1_EXTENDED);
        let driver = PiDriver::new();
        let source = source_for(dir.path());
        let mut native = source.open_native(V1_EXTENDED_ID).unwrap();

        let mut sink = MemorySink::default();
        let written = driver.write_native(native.as_mut(), &mut sink).unwrap();
        assert_eq!(written.content_format, CONTENT_FORMAT);
        assert_eq!(written.summary_version, SUMMARY_VERSION);
        assert_eq!(
            sink.0[CONTENT_PATH],
            fs::read(&path).unwrap(),
            "version 1 bytes, blank and torn lines included: the driver does not migrate"
        );
        let attrs = (written.attrs)().unwrap();
        assert_eq!(attrs.title(), Some("Rename the helper"));
        assert_eq!(attrs.project_name(), Some("-w-legacy"));
        assert_eq!(attrs.line_count(), Some(10));
        assert_eq!(attrs.extra_attrs()[FORMAT_VERSION_ATTR], 1);

        let content = String::from_utf8(sink.0[CONTENT_PATH].clone()).unwrap();
        let mut stored = driver
            .read_stored(
                V1_EXTENDED_ID.into(),
                CONTENT_FORMAT,
                Box::new(Stored(content)),
            )
            .unwrap();
        let entries: Vec<Entry> = stored.entries().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 9);
        let prompt = entries[1].message.as_ref().unwrap();
        assert_eq!(
            (prompt.message_type.as_str(), prompt.subtype.as_deref()),
            ("user", Some("text"))
        );
        let tool_uses: Vec<ToolUse> = stored.tool_uses().map(Result::unwrap).collect();
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].name, "read");
        assert_eq!((tool_uses[0].line, tool_uses[0].result_line), (3, Some(4)));
        assert_eq!(
            tool_uses[0].files[0].path,
            Path::new("/w/legacy/src/helper.py"),
            "a relative path resolves against the header's working directory"
        );
        assert_eq!(tool_uses[0].files[0].type_, FileType::Read);
        assert_eq!(stored.entry_values().count(), 9);
        let stored_attrs = stored.attrs().unwrap();
        assert_eq!(stored_attrs.message_count(), attrs.message_count());
        assert_eq!(stored_attrs.cost_usd(), attrs.cost_usd());
        assert_eq!(stored.summary_version(), SUMMARY_VERSION);
    }

    #[test]
    fn write_derived_applies_the_derivation() {
        let driver = PiDriver::new();
        let mut stored = driver
            .read_stored(
                "s".into(),
                CONTENT_FORMAT,
                Box::new(Stored(V3_BRANCH.to_string())),
            )
            .unwrap();
        let derivation = Derivation {
            dropped: [4, 5].into_iter().collect(),
            ..Derivation::default()
        };
        let mut sink = MemorySink::default();
        let appended = stored.write_derived(&derivation, &mut sink).unwrap();
        assert!(appended.is_empty());
        let derived = String::from_utf8(sink.0[CONTENT_PATH].clone()).unwrap();
        let expected: Vec<&str> = V3_BRANCH
            .lines()
            .enumerate()
            .filter(|(i, _)| !(3..=4).contains(i))
            .map(|(_, line)| line)
            .collect();
        assert_eq!(derived.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn read_stored_refuses_a_content_format_this_build_does_not_write() {
        let driver = PiDriver::new();
        let stored =
            |format: &str| driver.read_stored("s".into(), format, Box::new(Stored(String::new())));
        assert!(stored(CONTENT_FORMAT).is_ok());
        let error = stored("pi-jsonl 2").err().unwrap().to_string();
        assert!(error.contains("pi-jsonl 2"), "{error}");
        assert!(error.contains("upgrade gage"), "{error}");
        assert!(driver.codec("codex-rollout 1").is_err());
    }

    #[test]
    fn native_order_compares_the_format_version_and_then_the_line_count() {
        let driver = PiDriver::new();
        let summary = |version: Option<u64>, lines| {
            let mut summary = SummaryAttrs {
                line_count: Some(lines),
                ..SummaryAttrs::default()
            };
            if let Some(version) = version {
                let version = Value::from(version);
                summary
                    .extra
                    .insert(FORMAT_VERSION_ATTR.to_string(), version);
            }
            summary
        };
        let order = |native, stored| driver.native_order(&native, &stored);
        assert_eq!(
            order(summary(Some(3), 8), summary(Some(1), 10)),
            Ordering::Greater,
            "a migration is the later state though it is shorter"
        );
        assert_eq!(
            order(summary(Some(1), 12), summary(Some(3), 8)),
            Ordering::Less
        );
        assert_eq!(
            order(summary(Some(3), 9), summary(Some(3), 8)),
            Ordering::Greater
        );
        assert_eq!(
            order(summary(Some(3), 7), summary(Some(3), 8)),
            Ordering::Less
        );
        assert_eq!(
            order(summary(Some(3), 8), summary(Some(3), 8)),
            Ordering::Equal
        );
        assert_eq!(
            order(summary(None, 9), summary(Some(3), 8)),
            Ordering::Greater
        );
    }

    #[test]
    fn moving_and_deleting_a_session_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed(dir.path(), "kept", V1_PLAIN);
        let source = source_for(dir.path());
        assert!(source.delete_native("kept").is_err());
        assert!(source.move_native("kept", dir.path()).is_err());
        assert!(path.exists());
    }

    /// Every line Pi wrote, and every hand-built line that parses,
    /// comes back byte for byte from a decode and an encode. The lines
    /// Pi wrote hold costs such as `0.000005` and `5e-7`.
    #[test]
    fn the_codec_round_trips_every_line() {
        let codec = PiDriver::new().codec(CONTENT_FORMAT).unwrap();
        let written: Vec<String> = GENERATED.iter().map(|name| generated(name).1).collect();
        assert!(
            written
                .iter()
                .any(|text| text.contains(r#""cacheRead":5e-7"#))
        );
        assert!(
            written
                .iter()
                .any(|text| text.contains(r#""input":0.000005"#))
        );
        let built = [
            V1_EXTENDED,
            V1_PLAIN,
            V3_BRANCH,
            V3_HEADER_V1_ENTRIES,
            V3_TORN,
        ];
        let texts = written.iter().map(String::as_str).chain(built);
        for line in texts.flat_map(str::lines).filter(|l| !l.is_empty()) {
            let value = codec.decode(line.as_bytes()).unwrap();
            assert_eq!(codec.encode(&value).unwrap(), line.as_bytes(), "{line}");
        }
    }

    #[test]
    fn the_typed_sets_select_what_the_harness_and_the_machine_wrote() {
        let codec = PiDriver::new().codec(CONTENT_FORMAT).unwrap();
        let (_, run) = generated("run");
        let parsed = lines(&run);
        let dropped = |set: TypedSet| -> Vec<String> {
            let kept = run.lines().zip(&parsed).filter(|(text, _)| {
                let entry = codec.decode(text.as_bytes()).unwrap();
                codec.descriptor().typed_values(set, &entry).dropped
            });
            let mut kinds: Vec<String> = kept
                .map(|(_, line)| {
                    let role = line.pointer("/message/role").and_then(Value::as_str);
                    let kind = line["type"].as_str().unwrap();
                    role.map_or(kind.to_string(), |role| format!("{kind} {role}"))
                })
                .collect();
            kinds.sort();
            kinds.dedup();
            kinds
        };
        assert_eq!(
            dropped(TypedSet::HarnessContext),
            ["custom_message", "message system"]
        );
        assert_eq!(
            dropped(TypedSet::HarnessRecord),
            ["model_change", "thinking_level_change", "usage"],
            "the name a person gave the session stays"
        );

        let values = |line: &str, set: TypedSet| {
            let entry = codec.decode(line.as_bytes()).unwrap();
            codec.descriptor().typed_values(set, &entry)
        };
        let header = run.lines().next().unwrap();
        assert_eq!(
            values(header, TypedSet::System).literals,
            ["/tmp/gage-pi-fixtures/project"]
        );
        let compaction = run.lines().find(|l| l.contains(r#""type":"compaction""#));
        let context: Vec<String> = values(compaction.unwrap(), TypedSet::HarnessContext)
            .values
            .into_iter()
            .map(|hit| hit.pointer)
            .collect();
        assert_eq!(context, ["/systemMessage"]);
    }

    #[test]
    fn identifiers_and_signatures_are_not_text() {
        let codec = PiDriver::new().codec(CONTENT_FORMAT).unwrap();
        let line = r#"{"type":"message","id":"a0000001","parentId":null,"timestamp":"2026-01-02T03:04:05.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Plan it","thinkingSignature":"c2ln"},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"a.txt"}}],"provider":"anthropic","model":"claude-new","stopReason":"toolUse"}}"#;
        let entry = codec.decode(line.as_bytes()).unwrap();
        let text: Vec<String> = codec
            .descriptor()
            .text_values(&entry)
            .into_iter()
            .map(|value| value.text)
            .collect();
        assert_eq!(text, ["Plan it", "a.txt"]);
    }

    #[test]
    fn a_body_shows_what_happened() {
        let driver = PiDriver::new();
        let codec = driver.codec(CONTENT_FORMAT).unwrap();
        let (_, run) = generated("run");
        let body = |found: &dyn Fn(&&str) -> bool| {
            let text = run.lines().find(found).unwrap();
            let entry = codec.decode(text.as_bytes()).unwrap();
            driver.entry_body(CONTENT_FORMAT, &entry).blocks
        };
        let has = |part: &'static str| move |line: &&str| line.contains(part);

        assert_eq!(
            body(&has(r#""role":"user""#)),
            [BodyBlock::Markdown("What is in notes.txt?".into())]
        );
        let call = body(&has(r#""name":"read","arguments""#));
        assert_eq!(
            call[0],
            BodyBlock::Markdown("The notes file is the place to start.".into())
        );
        assert!(matches!(&call[1], BodyBlock::Attrs(attrs) if attrs[0].value == "read"));
        assert_eq!(
            call[2],
            BodyBlock::Text("{\n  \"path\": \"notes.txt\"\n}".into())
        );
        let result = body(&has(r#""role":"toolResult""#));
        assert_eq!(
            result.last(),
            Some(&BodyBlock::Text("alpha\nbeta\ngamma\n".into()))
        );
        let system = body(&has(r#""toolsRemoved""#));
        assert!(
            system.contains(&BodyBlock::Heading("tools".into())),
            "{system:?}"
        );
        assert_eq!(
            system.last(),
            Some(&BodyBlock::Remark("1 tools removed".into()))
        );
        let shell = body(&has(r#""role":"bashExecution""#));
        assert!(
            matches!(&shell[0], BodyBlock::Attrs(attrs) if attrs[0].value == "echo delta >> notes.txt"),
            "{shell:?}"
        );
        let usage = body(&has(r#""kind":"gage_fixture""#));
        assert!(
            matches!(&usage[0], BodyBlock::Attrs(attrs) if attrs.last().unwrap().value == "0.000005500000000000001"),
            "{usage:?}"
        );
        let compaction = body(&has(r#""type":"compaction""#));
        assert!(matches!(&compaction[1], BodyBlock::Markdown(text) if text.starts_with("## Goal")));

        let (_, branches) = generated("branches");
        let edit = branches
            .lines()
            .find(|l| l.contains(r#""replacement":null"#));
        let entry = codec.decode(edit.unwrap().as_bytes()).unwrap();
        assert_eq!(
            driver.entry_body(CONTENT_FORMAT, &entry).blocks.last(),
            Some(&BodyBlock::Remark("Left out of the model's context".into()))
        );

        let torn = codec
            .decode(V3_TORN.lines().nth(2).unwrap().as_bytes())
            .unwrap();
        assert_eq!(driver.entry_body(CONTENT_FORMAT, &torn).blocks.len(), 2);
        assert!(
            driver
                .entry_body("other 1", &codec.decode(b"{}").unwrap())
                .blocks
                .is_empty()
        );
    }

    #[test]
    fn the_schemas_describe_the_format_the_driver_writes() {
        let schemas = PiDriver::new().content_schemas();
        assert_eq!(schemas.len(), 3);
        assert!(schemas.iter().all(|s| s.content_format == CONTENT_FORMAT));
    }
}
