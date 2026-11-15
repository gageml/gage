//! `CodexDriver` implements `gage_session::Driver` for Codex sessions
//! stored under a Codex home as
//! `sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`.
//!
//! Source grammar, with or without the `codex:` scheme:
//! - `` (empty) -- default location: `$CODEX_HOME`, else `$HOME/.codex`
//! - `<path>` -- an explicit Codex home; sessions live under
//!   `<path>/sessions`
//!
//! A session's own URL is `codex:<absolute home>/<thread id>`, always
//! with the resolved home, so the same file read through any spelling
//! of its source records the same `native_source`.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use gage_core::config::default_cache_root;
use gage_corpus::tables::{EntryTable, MessageTable, SessionTable, ToolUseFileTable, ToolUseTable};
use gage_corpus::{
    CorpusSessionAttrs, Fingerprint, IndexStore, LockMode, SessionInfo, SessionReader,
    decoded_entries, format_project_slug, project_slug, write_derived,
};
use gage_session::{
    ContentSink, ContentSource, DecodedEntry, Derivation, Driver, DriverError, DriverTables, Entry,
    EntryBody, EntryCodec, EntryValue, MaintenanceOutcome, MaintenanceStatus, MessageTextFilter,
    MessageTextHit, NativeLookupError, NativeSession, PatchHunk, SessionAttrs, Source,
    SourceOptions, StoredSession, SummaryAttrs, ToolUse, WrittenContent, split_scheme,
};
use serde_json::Value;

use crate::body;
use crate::codec::codec;
use crate::corpus::CodexCorpus;
use crate::derive::{SUMMARY_VERSION, derive_entry, derive_session_from};
use crate::home::codex_home;
use crate::patch;
use crate::rollout::read_meta;
use crate::tool_use::ToolUseCollector;

pub(crate) const NAME: &str = "codex";
const SCHEMES: &[&str] = &["codex"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const SESSION_TYPE: &str = "codex";
/// `content_format` value returned by `write_native` and expected by
/// `read_stored`. The name identifies the byte layout under
/// `files.d/`; the trailing digit is its version.
pub(crate) const CONTENT_FORMAT: &str = "codex-rollout 1";
/// The path of the rollout in a stored session's content
const CONTENT_PATH: &str = "rollout.jsonl";

pub struct CodexDriver;

impl CodexDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for CodexDriver {
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
        Ok(Box::new(CodexSource::open(options)?))
    }

    /// A session URL is `codex:<home>/<id>`; its source is
    /// `codex:<home>`. A bare id is a session in the default
    /// location, `codex:`.
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
        let (home, id) = body.rsplit_once('/').unwrap_or(("", body));
        if id.is_empty() {
            return Err(DriverError::Other(format!(
                "{session_url}: session URL has no session id"
            )));
        }
        Ok((format!("{NAME}:{home}"), id.to_string()))
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

    fn patch_hunks(&self, result_raw: &str) -> Vec<PatchHunk> {
        patch::hunks(result_raw)
    }

    /// The rollout is stored as it is read here. A change to the file
    /// after the read is not included. One read serves the write and
    /// the derive, so the derived attributes and the size describe the
    /// bytes stored. The mtime is the one read when the session was
    /// opened.
    fn write_native(
        &self,
        session: &mut dyn NativeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<WrittenContent, DriverError> {
        let codex = session
            .as_any()
            .downcast_ref::<CodexNativeSession>()
            .ok_or_else(|| {
                DriverError::Other("write_native: session is not a CodexNativeSession".into())
            })?;
        let info = codex.info.clone();
        let bytes = fs::read(&info.src).map_err(DriverError::Io)?;
        sink.create(CONTENT_PATH)
            .and_then(|mut w| w.write_all(&bytes))
            .map_err(DriverError::Io)?;
        let fingerprint = Fingerprint {
            mtime_ms: millis(info.mtime),
            size: bytes.len() as u64,
        };
        let store = codex.store.clone();
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
                // The cache is rebuilt from the rollout on any read
                // that misses it, so a failed write costs a later read
                // and is logged rather than raised.
                if let Err(e) = store.put_session_attrs(&info.id, &derived.attrs) {
                    tracing::warn!(session_id = %info.id, "summary cache write failed: {e}");
                }
                let mut attrs = derived.attrs;
                if let Some(title) = store.corpus().recorded_title(&info.id) {
                    attrs.title = Some(title);
                }
                Ok(Box::new(attrs) as Box<dyn SessionAttrs>)
            },
        ))
    }

    /// A rollout grows by lines: more lines is a later state, fewer
    /// is an older copy, and the same count with other bytes is a
    /// modification in place.
    fn native_order(&self, native: &SummaryAttrs, stored: &SummaryAttrs) -> Ordering {
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
        Ok(Box::new(CodexStoredSession {
            native_id,
            content_format: content_format.to_string(),
            source,
        }))
    }
}

/// Handle over one Codex home. Holds the index store for the home's
/// rollouts.
pub struct CodexSource {
    source: String,
    /// The Codex home, absolute
    home: PathBuf,
    store: Arc<IndexStore>,
    /// Thread id to session, walked once: one walk serves every
    /// lookup of the handle.
    sessions: OnceLock<Result<HashMap<String, SessionInfo>, String>>,
}

impl CodexSource {
    fn open(options: &SourceOptions) -> Result<Self, DriverError> {
        let source = options.url();
        let home = resolve_home(source)?;
        let cache_root = options
            .cache_root_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_cache_root);
        let corpus = Arc::new(CodexCorpus::new(home.clone()));
        let store =
            Arc::new(IndexStore::new(corpus, cache_root).show_agents(options.shows_agents()));
        store.ensure_summary_dir()?;
        Ok(Self {
            source: source.to_string(),
            home,
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

    /// The session `native_id` with a fresh stat of its rollout. The
    /// stat is taken per call so a file removed after the walk reports
    /// not found and the mtime reflects the file as it is now.
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

impl Source for CodexSource {
    fn source(&self) -> &str {
        &self.source
    }

    fn session_url(&self, session_id: &str) -> String {
        session_url(&self.home, session_id)
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
        Ok(Box::new(CodexNativeSession {
            source: self.session_url(native_id),
            info: self.session(native_id)?,
            store: Arc::clone(&self.store),
            attrs: OnceCell::new(),
        }))
    }

    fn delete_native(&self, _native_id: &str) -> Result<(), DriverError> {
        Err(DriverError::Other(
            "deleting a Codex session is not supported".into(),
        ))
    }

    fn move_native(&self, _native_id: &str, _project: &Path) -> Result<(), DriverError> {
        Err(DriverError::Other(
            "moving a Codex session is not supported".into(),
        ))
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
    /// its rollout records it.
    fn project_path(&self, name: &str) -> Result<Option<PathBuf>, DriverError> {
        let Some(session) = self.sessions()?.values().find(|s| s.project == name) else {
            return Ok(None);
        };
        Ok(read_meta(&session.src)?.cwd)
    }

    fn close(self: Box<Self>) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Resolve the Codex home from a source value. A leading `codex:`
/// scheme is stripped; any other scheme is an error. An empty body is
/// the default location. A non-empty body is a Codex home path (`~`
/// expanded).
fn resolve_home(source: &str) -> Result<PathBuf, DriverError> {
    let body = match split_scheme(source) {
        Some((scheme, body)) if SCHEMES.contains(&scheme) => body,
        Some((scheme, _)) => {
            return Err(DriverError::Other(format!(
                "unsupported source scheme {scheme:?}: {source}"
            )));
        }
        None => source,
    };
    let home = if body.is_empty() {
        codex_home().ok_or_else(|| DriverError::Other("CODEX_HOME or HOME must be set".into()))?
    } else {
        expand_tilde(body)
    };
    // Absolute so a session URL spelled from the home is location
    // independent
    std::path::absolute(&home).map_err(DriverError::Io)
}

fn expand_tilde(s: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (s.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        (None, Some(home)) if s == "~" => home,
        _ => PathBuf::from(s),
    }
}

/// `codex:<absolute home>/<id>`, the URL of one session under `home`
fn session_url(home: &Path, session_id: &str) -> String {
    format!("{NAME}:{}/{session_id}", home.display())
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// A native Codex session. Its attributes come from the source's
/// summary cache when the cache entry is current for the rollout's
/// mtime, else from one parse of the rollout, which then refreshes
/// the cache.
pub struct CodexNativeSession {
    /// `codex:<absolute home>/<thread id>`
    source: String,
    /// The rollout as the caller last read it from the filesystem,
    /// before the session was opened
    info: SessionInfo,
    store: Arc<IndexStore>,
    /// The attributes, read on first use
    attrs: OnceCell<CorpusSessionAttrs>,
}

impl NativeSession for CodexNativeSession {
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

struct CodexStoredSession {
    native_id: String,
    content_format: String,
    source: Box<dyn ContentSource>,
}

impl CodexStoredSession {
    fn reader(&self) -> io::Result<SessionReader<Box<dyn Read + Send>>> {
        Ok(SessionReader::new(self.source.open(CONTENT_PATH)?, codec()))
    }
}

impl StoredSession for CodexStoredSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }

    fn content_format(&self) -> &str {
        &self.content_format
    }

    /// One normalized row per parsed line of the rollout, through the
    /// same derivation the native tables use. A blank line and a line
    /// without a type consume a line number and yield no row; a line
    /// that does not read or decode is an error naming the line.
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

    /// The tool calls of the rollout, through the same collector the
    /// native table uses. A call's result line is known only once its
    /// output is read, so the rows are read before the first is
    /// yielded; read failures follow the rows.
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

    /// The rollout's attributes through the same derivation a native
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

    use gage_session::{BodyBlock, FileType, TypedSet};

    use super::*;
    use crate::testing::{ROLLOUT, THREAD_ID, seed};

    const OTHER_ID: &str = "00000000-0000-7000-8000-000000000002";

    fn options_for(home: &Path) -> SourceOptions {
        let mut options = SourceOptions::new(&format!("codex:{}", home.display()));
        options.cache_root(home.join("cache"));
        options
    }

    fn source_for(home: &Path) -> Box<dyn Source> {
        CodexDriver::new().open_source(&options_for(home)).unwrap()
    }

    /// The fixture rollout as stored content
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
    fn a_listing_holds_the_threads_people_started() {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), THREAD_ID, "user");
        seed(home.path(), OTHER_ID, "gage-scan");

        let source = source_for(home.path());
        assert_eq!(source.find_native("0000").unwrap(), THREAD_ID);
        assert!(matches!(
            source.find_native(OTHER_ID),
            Err(NativeLookupError::NoMatch(_))
        ));

        let mut options = options_for(home.path());
        options.show_agents(true);
        let with_agents = CodexDriver::new().open_source(&options).unwrap();
        assert_eq!(with_agents.find_native(OTHER_ID).unwrap(), OTHER_ID);
        assert!(matches!(
            with_agents.find_native("0000"),
            Err(NativeLookupError::TooManyMatches { .. })
        ));
    }

    #[test]
    fn a_native_session_is_named_by_home_and_thread_id() {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), THREAD_ID, "user");
        let source = source_for(home.path());
        let session = source.open_native(THREAD_ID).unwrap();
        let url = format!("codex:{}/{THREAD_ID}", home.path().display());
        assert_eq!(session.source(), url);
        assert_eq!(session.id(), THREAD_ID);
        assert_eq!(session.session_type(), "codex");

        let (source_url, id) = CodexDriver::new().split_session_url(&url).unwrap();
        assert_eq!(source_url, format!("codex:{}", home.path().display()));
        assert_eq!(id, THREAD_ID);
        assert!(CodexDriver::new().split_session_url("claude:/x/y").is_err());
    }

    #[test]
    fn the_project_is_the_slug_of_the_directory_the_session_started_in() {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), THREAD_ID, "user");
        let source = source_for(home.path());
        let session = source.open_native(THREAD_ID).unwrap();
        assert_eq!(session.attrs().project_name(), Some("-home-u-proj"));
        assert_eq!(
            source.project_name(Path::new("/home/u/proj")).unwrap(),
            "-home-u-proj"
        );
        assert_eq!(
            source.project_path("-home-u-proj").unwrap(),
            Some(PathBuf::from("/home/u/proj"))
        );
        assert_eq!(source.project_path("-no-such").unwrap(), None);
        assert_eq!(
            CodexDriver::new().format_project("-home-u-proj", 25),
            "proj"
        );
    }

    #[test]
    fn a_recorded_thread_name_is_the_title() {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), THREAD_ID, "user");
        let source = source_for(home.path());
        let derived = source.open_native(THREAD_ID).unwrap();
        assert_eq!(derived.attrs().title(), Some("Fix the greeting"));

        fs::write(
            home.path().join("session_index.jsonl"),
            format!(r#"{{"id":"{THREAD_ID}","thread_name":"Greeting fix","updated_at":"2026-01-02T03:05:00Z"}}"#),
        )
        .unwrap();
        let named = source.open_native(THREAD_ID).unwrap();
        assert_eq!(
            named.attrs().title(),
            Some("Greeting fix"),
            "the name is read past the cached summary"
        );
    }

    #[test]
    fn write_native_stores_the_rollout_as_it_is_and_reads_it_back() {
        let home = tempfile::tempdir().unwrap();
        let path = seed(home.path(), THREAD_ID, "user");
        let driver = CodexDriver::new();
        let source = source_for(home.path());
        let mut native = source.open_native(THREAD_ID).unwrap();

        let mut sink = MemorySink::default();
        let written = driver.write_native(native.as_mut(), &mut sink).unwrap();
        assert_eq!(written.content_format, CONTENT_FORMAT);
        assert_eq!(written.summary_version, SUMMARY_VERSION);
        assert_eq!(sink.0[CONTENT_PATH], fs::read(&path).unwrap());
        let attrs = (written.attrs)().unwrap();
        assert_eq!(attrs.title(), Some("Fix the greeting"));
        assert_eq!(attrs.project_name(), Some("-home-u-proj"));
        assert_eq!(attrs.line_count(), Some(23));

        let content = String::from_utf8(sink.0[CONTENT_PATH].clone()).unwrap();
        let mut stored = driver
            .read_stored(THREAD_ID.into(), CONTENT_FORMAT, Box::new(Stored(content)))
            .unwrap();
        let entries: Vec<Entry> = stored.entries().map(Result::unwrap).collect();
        assert_eq!(entries.len(), 23);
        let prompt = entries[6].message.as_ref().unwrap();
        assert_eq!(
            (prompt.message_type.as_str(), prompt.subtype.as_deref()),
            ("user", Some("text"))
        );
        let tool_uses: Vec<ToolUse> = stored.tool_uses().map(Result::unwrap).collect();
        let names: Vec<&str> = tool_uses.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["exec_command", "apply_patch", "mcp__gage__query"]);
        assert!(
            tool_uses[1]
                .files
                .iter()
                .all(|f| f.type_ == FileType::Write)
        );
        assert_eq!(stored.entry_values().count(), 23);
        let stored_attrs = stored.attrs().unwrap();
        assert_eq!(stored_attrs.message_count(), attrs.message_count());
        assert_eq!(stored_attrs.input_tokens(), Some(300));
        assert_eq!(stored.summary_version(), SUMMARY_VERSION);
    }

    #[test]
    fn write_derived_applies_the_derivation() {
        let driver = CodexDriver::new();
        let mut stored = driver
            .read_stored(
                THREAD_ID.into(),
                CONTENT_FORMAT,
                Box::new(Stored(ROLLOUT.to_string())),
            )
            .unwrap();
        let derivation = Derivation {
            dropped: [2, 3].into_iter().collect(),
            ..Derivation::default()
        };
        let mut sink = MemorySink::default();
        assert!(
            stored
                .write_derived(&derivation, &mut sink)
                .unwrap()
                .is_empty()
        );
        let derived = String::from_utf8(sink.0[CONTENT_PATH].clone()).unwrap();
        let expected: Vec<&str> = ROLLOUT
            .lines()
            .enumerate()
            .filter(|(i, _)| !(1..=2).contains(i))
            .map(|(_, line)| line)
            .collect();
        assert_eq!(derived.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn read_stored_refuses_a_content_format_this_build_does_not_write() {
        let driver = CodexDriver::new();
        let stored =
            |format: &str| driver.read_stored("s".into(), format, Box::new(Stored(String::new())));
        assert!(stored(CONTENT_FORMAT).is_ok());
        let error = stored("codex-rollout 2").err().unwrap().to_string();
        assert!(error.contains("codex-rollout 2"), "{error}");
        assert!(error.contains("upgrade gage"), "{error}");
        assert!(driver.codec("claude-jsonl 1").is_err());
    }

    #[test]
    fn native_order_compares_line_counts() {
        let driver = CodexDriver::new();
        let lines = |n| SummaryAttrs {
            line_count: Some(n),
            ..SummaryAttrs::default()
        };
        assert_eq!(driver.native_order(&lines(5), &lines(3)), Ordering::Greater);
        assert_eq!(driver.native_order(&lines(3), &lines(5)), Ordering::Less);
        assert_eq!(driver.native_order(&lines(3), &lines(3)), Ordering::Equal);
    }

    #[test]
    fn moving_and_deleting_a_session_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let path = seed(home.path(), THREAD_ID, "user");
        let source = source_for(home.path());
        assert!(source.delete_native(THREAD_ID).is_err());
        assert!(source.move_native(THREAD_ID, home.path()).is_err());
        assert!(path.exists());
    }

    /// Every line of the fixture comes back byte for byte from a
    /// decode and an encode.
    #[test]
    fn the_codec_round_trips_a_rollout() {
        let codec = CodexDriver::new().codec(CONTENT_FORMAT).unwrap();
        for line in ROLLOUT.lines() {
            let value = codec.decode(line.as_bytes()).unwrap();
            assert_eq!(codec.encode(&value).unwrap(), line.as_bytes(), "{line}");
        }
    }

    #[test]
    fn the_typed_sets_select_what_the_harness_and_the_machine_wrote() {
        let codec = CodexDriver::new().codec(CONTENT_FORMAT).unwrap();
        let values = |line: usize, set: TypedSet| {
            let text = ROLLOUT.lines().nth(line - 1).unwrap();
            let entry = codec.decode(text.as_bytes()).unwrap();
            codec.descriptor().typed_values(set, &entry)
        };
        let dropped = |set: TypedSet| -> Vec<usize> {
            (1..=23).filter(|&line| values(line, set).dropped).collect()
        };
        assert_eq!(
            dropped(TypedSet::HarnessContext),
            [3, 4, 5],
            "the tool list, the developer message, and the context under the user role"
        );
        assert_eq!(
            dropped(TypedSet::HarnessRecord),
            [2, 6, 13, 21, 22, 23],
            "an item_completed event records a tool call and stays"
        );
        let meta = values(1, TypedSet::Pii);
        assert_eq!(
            meta.literals,
            [
                "user-FAKE0000000000000000",
                "00000000-0000-4000-8000-00000000acc7"
            ]
        );
        assert_eq!(values(1, TypedSet::System).literals, ["/home/u/proj"]);
        let context: Vec<String> = values(1, TypedSet::HarnessContext)
            .values
            .into_iter()
            .map(|hit| hit.pointer)
            .collect();
        assert_eq!(context, ["/payload/base_instructions"]);
    }

    #[test]
    fn opaque_fields_are_not_text() {
        let codec = CodexDriver::new().codec(CONTENT_FORMAT).unwrap();
        let reasoning = ROLLOUT.lines().nth(8).unwrap();
        let entry = codec.decode(reasoning.as_bytes()).unwrap();
        let text: Vec<String> = codec
            .descriptor()
            .text_values(&entry)
            .into_iter()
            .map(|value| value.text)
            .collect();
        assert_eq!(text, ["Plan the edit"], "not the encrypted content");
    }

    #[test]
    fn a_body_shows_what_happened() {
        let driver = CodexDriver::new();
        let codec = driver.codec(CONTENT_FORMAT).unwrap();
        let body = |line: usize| {
            let text = ROLLOUT.lines().nth(line - 1).unwrap();
            let entry = codec.decode(text.as_bytes()).unwrap();
            driver.entry_body(CONTENT_FORMAT, &entry).blocks
        };
        assert_eq!(
            body(7),
            [BodyBlock::Markdown("Fix the greeting\nplease".into())]
        );
        assert_eq!(body(9), [BodyBlock::Markdown("Plan the edit".into())]);
        let command = body(14);
        assert!(
            matches!(&command[0], BodyBlock::Attrs(attrs) if attrs[0].value.starts_with("printf")),
            "{command:?}"
        );
        assert_eq!(command[1], BodyBlock::Text("buy milk\n".into()));
        let change = body(17);
        assert!(change.contains(&BodyBlock::Heading("update /home/u/proj/hello.txt".into())));
        assert!(change.contains(&BodyBlock::Text("@@ -1 +1 @@\n-hello\n+goodbye\n".into())));
        let mcp = body(18);
        assert_eq!(
            mcp.last(),
            Some(&BodyBlock::Text(
                "MCP tool call requires approval, but approval policy is never".into()
            ))
        );
        assert!(
            driver
                .entry_body("other 1", &codec.decode(b"{}").unwrap())
                .blocks
                .is_empty()
        );
    }
}
