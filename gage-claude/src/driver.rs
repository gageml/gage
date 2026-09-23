//! `ClaudeDriver` implements `gage_session::Driver` for Claude Code
//! sessions stored under a projects directory as `<slug>/<uuid>.jsonl`.
//!
//! Source grammar, with or without the `claude:` scheme:
//! - `` (empty) -- default location: `$CLAUDE_PROJECTS_DIR`, else
//!   `projects/` under `$CLAUDE_CONFIG_DIR` or `$HOME/.claude`
//! - `<path>` -- explicit filesystem root; sessions live under
//!   `<path>/projects/**/*.jsonl`
//!
//! A session's own URL is `claude:<absolute root>/<uuid>`, always with
//! the resolved root, so the same file read through any spelling of
//! its source records the same `native_source`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use gage_session::{
    ContentSink, ContentSource, Driver, DriverError, DriverTables, Entry, NativeLookupError,
    NativeSession, SessionAttrs, Source, StoredSession, split_scheme,
};

use crate::home::ClaudeHome;
use crate::index::{IndexStore, SessionSummary, cache_dir_for, derive_session};
use crate::session::{
    SESSION_RE, delete_session, encode_project_dir, is_agent_tmp_slug, projects_dir,
};
use crate::tables::{EntryTable, MessageTable, SessionTable};

const NAME: &str = "claude";
const SCHEMES: &[&str] = &["claude"];
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SESSION_TYPE: &str = "claude";
/// `content_format` value returned by `write_native` and expected by
/// `read_stored`. The name identifies the byte layout under
/// `files.d/`; the trailing digit is its version.
const CONTENT_FORMAT: &str = "claude-jsonl 1";

pub struct ClaudeDriver;

impl ClaudeDriver {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for ClaudeDriver {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> &'static str {
        VERSION
    }

    fn schemes(&self) -> &'static [&'static str] {
        SCHEMES
    }

    fn open_source(&self, source: &str) -> Result<Box<dyn Source>, DriverError> {
        Ok(Box::new(ClaudeSource::open(source)?))
    }

    /// Drops the `claude-` vendor prefix every Claude model name carries
    fn format_model(&self, model: &str) -> String {
        model.strip_prefix("claude-").unwrap_or(model).to_string()
    }

    fn format_project(&self, name: &str, max_chars: usize) -> String {
        shorten_project_name(strip_home_prefix(name), max_chars)
    }

    fn write_native(
        &self,
        session: &mut dyn NativeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<String, DriverError> {
        let claude = session
            .as_any()
            .downcast_ref::<ClaudeNativeSession>()
            .ok_or_else(|| {
                DriverError::Other("write_native: session is not a ClaudeNativeSession".into())
            })?;
        // Only the session transcript is stored. Subagent and sidecar
        // content is intentionally left out until a consumer exists for
        // it; a session is re-added over time, so that content can join
        // later at no duplication cost (git shares identical blobs).
        let session_path = claude.session_path().to_path_buf();
        copy_file_into_sink(&session_path, "session.jsonl", sink)?;
        Ok(CONTENT_FORMAT.to_string())
    }

    fn read_stored(
        &self,
        native_id: String,
        content_format: &str,
        source: Box<dyn ContentSource>,
    ) -> Result<Box<dyn StoredSession>, DriverError> {
        Ok(Box::new(ClaudeStoredSession {
            native_id,
            content_format: content_format.to_string(),
            source,
        }))
    }
}

/// Handle over one Claude projects directory. Holds the index store
/// for the directory and the project registry parsed from
/// `.claude.json` on first use.
pub struct ClaudeSource {
    source: String,
    projects_dir: PathBuf,
    /// The Claude root the projects dir sits under
    root: PathBuf,
    store: Arc<IndexStore>,
    /// Project slug to recorded cwd, loaded once. The error string is
    /// kept so every later call reports the same failure.
    projects: OnceLock<Result<HashMap<String, PathBuf>, String>>,
}

impl ClaudeSource {
    fn open(source: &str) -> Result<Self, DriverError> {
        let projects_dir = resolve_projects_dir(source)?;
        let root = root_of(&projects_dir);
        let cache_dir = cache_dir_for(&projects_dir);
        let store = Arc::new(IndexStore::new(projects_dir.clone(), cache_dir));
        store.ensure_cache_dir()?;
        Ok(Self {
            source: source.to_string(),
            projects_dir,
            root,
            store,
            projects: OnceLock::new(),
        })
    }

    /// The index store for this source's projects directory: the
    /// summary cache and the text index.
    pub fn index_store(&self) -> Arc<IndexStore> {
        Arc::clone(&self.store)
    }

    /// The Claude root the projects directory sits under
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn projects(&self) -> Result<&HashMap<String, PathBuf>, DriverError> {
        self.projects
            .get_or_init(|| load_projects(&self.root).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| DriverError::Other(format!("reading project registry: {e}")))
    }
}

impl Source for ClaudeSource {
    fn source(&self) -> &str {
        &self.source
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn tables(&self) -> Result<DriverTables, DriverError> {
        Ok(DriverTables {
            session: Arc::new(SessionTable::new(Arc::clone(&self.store))),
            message: Arc::new(MessageTable::new(Arc::clone(&self.store))),
            entry: Arc::new(EntryTable::new(Arc::clone(&self.store))),
        })
    }

    fn find_native(&self, prefix: &str) -> Result<String, NativeLookupError> {
        let mut matches = Vec::new();
        for hit in walk_session_files(&self.projects_dir) {
            let (id, _path, _meta) = match hit {
                Ok(h) => h,
                Err(e) => return Err(NativeLookupError::Driver(DriverError::Io(e))),
            };
            if id.starts_with(prefix) {
                matches.push(id);
            }
        }
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
        let (path, meta) = self.session_file(native_id)?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        Ok(Box::new(ClaudeNativeSession::open(
            native_id,
            &path,
            mtime,
            meta.len(),
            &self.root,
            &self.store,
        )?))
    }

    fn delete_native(&self, native_id: &str) -> Result<(), DriverError> {
        let (path, _meta) = self.session_file(native_id)?;
        Ok(delete_session(&path)?)
    }

    fn project_name(&self, path: &Path) -> Result<String, DriverError> {
        let canonical = match fs::canonicalize(path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
            Err(e) => return Err(DriverError::Io(e)),
        };
        Ok(encode_project_dir(&canonical))
    }

    fn project_path(&self, name: &str) -> Result<Option<PathBuf>, DriverError> {
        Ok(self.projects()?.get(name).cloned())
    }

    fn close(self: Box<Self>) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Slug to cwd for every project the root's registry records. A
/// missing registry is an empty map.
/// The project name without a leading `-home-<user>-` or
/// `-Users-<user>-`, the encoded home directory Claude Code puts in
/// front of nearly every project. The match is on the name's shape,
/// so names written on another machine shorten the same way.
fn strip_home_prefix(name: &str) -> &str {
    ["-home-", "-Users-"]
        .iter()
        .find_map(|prefix| {
            let rest = name.strip_prefix(prefix)?;
            let (user, project) = rest.split_once('-')?;
            (!user.is_empty() && !project.is_empty()).then_some(project)
        })
        .unwrap_or(name)
}

/// The name cut to `max_chars` characters. A long name keeps a head
/// of one quarter of the budget, then `…`, then as many of its final
/// characters as fill the rest. The tail carries the project's own
/// directory name, so it gets the larger share.
fn shorten_project_name(name: &str, max_chars: usize) -> String {
    const ELLIPSIS: char = '…';
    let count = name.chars().count();
    if count <= max_chars {
        return name.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let head_len = max_chars / 4;
    let tail_len = max_chars - head_len - 1;
    let head: String = name.chars().take(head_len).collect();
    let tail: String = name.chars().skip(count - tail_len).collect();
    format!("{head}{ELLIPSIS}{tail}")
}

impl ClaudeSource {
    /// The session file for `native_id` and its metadata
    fn session_file(&self, native_id: &str) -> Result<(PathBuf, fs::Metadata), DriverError> {
        for hit in walk_session_files(&self.projects_dir) {
            let (id, path, meta) = hit?;
            if id == native_id {
                return Ok((path, meta));
            }
        }
        Err(DriverError::Other(format!(
            "native session not found: {NAME}:{native_id}"
        )))
    }
}

fn load_projects(root: &Path) -> io::Result<HashMap<String, PathBuf>> {
    let home = claude_home_for_root(root)?;
    let projects = match home.projects() {
        Ok(list) => list,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(projects
        .into_iter()
        .map(|p| (encode_project_dir(&p.path), p.path))
        .collect())
}

fn copy_file_into_sink(
    src: &Path,
    dest_key: &str,
    sink: &mut dyn ContentSink,
) -> Result<(), DriverError> {
    let mut writer = sink.create(dest_key).map_err(DriverError::Io)?;
    let mut file = File::open(src).map_err(DriverError::Io)?;
    io::copy(&mut file, &mut writer).map_err(DriverError::Io)?;
    Ok(())
}

/// Resolve the projects directory from a source value. A leading
/// `claude:` scheme is stripped; any other scheme is an error. An
/// empty body is the default location (`CLAUDE_PROJECTS_DIR`, else
/// `projects/` under the Claude home). A non-empty body is a Claude
/// root path (`~` expanded) whose sessions live under `projects/`.
fn resolve_projects_dir(source: &str) -> Result<PathBuf, DriverError> {
    let body = match split_scheme(source) {
        Some((scheme, body)) if SCHEMES.contains(&scheme) => body,
        Some((scheme, _)) => {
            return Err(DriverError::Other(format!(
                "unsupported source scheme {scheme:?}: {source}"
            )));
        }
        None => source,
    };
    let projects_dir = if body.is_empty() {
        projects_dir().ok_or_else(|| {
            DriverError::Other("CLAUDE_PROJECTS_DIR, CLAUDE_CONFIG_DIR, or HOME must be set".into())
        })?
    } else {
        expand_tilde(body).join("projects")
    };
    // Absolute so a session URL spelled from the root is location
    // independent
    std::path::absolute(&projects_dir).map_err(DriverError::Io)
}

/// The Claude root a projects directory sits under
fn root_of(projects_dir: &Path) -> PathBuf {
    projects_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| projects_dir.to_path_buf())
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if s == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(s)
}

fn walk_session_files(
    projects_dir: &Path,
) -> Box<dyn Iterator<Item = io::Result<(String, PathBuf, fs::Metadata)>> + '_> {
    let entries = match fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Box::new(std::iter::empty());
        }
        Err(e) => return Box::new(std::iter::once(Err(e))),
    };
    let it = entries.flat_map(|project_entry| {
        let project_path = match project_entry {
            Ok(e) => e.path(),
            Err(e) => return Box::new(std::iter::once(Err(e))) as _,
        };
        if !project_path.is_dir() {
            return Box::new(std::iter::empty()) as _;
        }
        let slug = project_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if is_agent_tmp_slug(&slug) {
            return Box::new(std::iter::empty()) as _;
        }
        let dir_entries = match fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Box::new(std::iter::empty()) as _;
            }
            Err(e) => return Box::new(std::iter::once(Err(e))) as _,
        };
        Box::new(dir_entries.filter_map(|f| {
            let path = match f {
                Ok(e) => e.path(),
                Err(e) => return Some(Err(e)),
            };
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned())?;
            if !SESSION_RE.is_match(&name) {
                return None;
            }
            let id = name[..36].to_string();
            let meta = match path.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
                Err(e) => return Some(Err(e)),
            };
            Some(Ok((id, path, meta)))
        })) as Box<dyn Iterator<Item = io::Result<(String, PathBuf, fs::Metadata)>>>
    });
    Box::new(it)
}

/// A native Claude session with its attributes read at open. The
/// summary (title, model, message count, emptiness, token usage) comes
/// from the source's summary cache when the cache entry is current for
/// the file's mtime, else from one parse of the transcript, which then
/// refreshes the cache.
pub struct ClaudeNativeSession {
    native_id: String,
    /// `claude:<absolute root>/<uuid>`
    source: String,
    session_path: PathBuf,
    attrs: ClaudeSessionAttrs,
}

impl ClaudeNativeSession {
    /// Open the session at `path`. `mtime` and `size` are the file's
    /// stat values the caller already holds from its directory walk;
    /// `root` is the absolute Claude root the file sits under.
    pub fn open(
        native_id: &str,
        path: &Path,
        mtime: SystemTime,
        size: u64,
        root: &Path,
        store: &IndexStore,
    ) -> Result<Self, DriverError> {
        let summary = match store.session_summary(native_id, mtime) {
            Some(cached) => cached,
            None => {
                let derived = derive_session(native_id, path)
                    .map_err(|e| DriverError::Other(format!("{}: {e}", path.display())))?;
                store.put_session_summary(native_id, &derived.summary)?;
                derived.summary
            }
        };
        let project_slug = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            native_id: native_id.to_string(),
            source: format!("{NAME}:{}/{native_id}", root.display()),
            session_path: path.to_path_buf(),
            attrs: ClaudeSessionAttrs {
                mtime,
                size,
                project_slug,
                summary,
            },
        })
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// The full derived summary, including the token counts the
    /// generic [`SessionAttrs`] view does not carry.
    pub fn summary(&self) -> &SessionSummary {
        &self.attrs.summary
    }
}

impl NativeSession for ClaudeNativeSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }

    fn session_type(&self) -> &str {
        SESSION_TYPE
    }

    fn source(&self) -> &str {
        &self.source
    }

    fn attrs(&self) -> &dyn SessionAttrs {
        &self.attrs
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct ClaudeSessionAttrs {
    mtime: SystemTime,
    size: u64,
    project_slug: String,
    summary: SessionSummary,
}

impl SessionAttrs for ClaudeSessionAttrs {
    fn mtime(&self) -> Option<SystemTime> {
        Some(self.mtime)
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }

    fn is_empty(&self) -> Option<bool> {
        Some(self.summary.is_empty)
    }

    fn project_name(&self) -> Option<&str> {
        Some(&self.project_slug)
    }

    fn title(&self) -> Option<&str> {
        self.summary.title.as_deref()
    }

    fn model(&self) -> Option<&str> {
        self.summary.model.as_deref()
    }

    fn message_count(&self) -> Option<u64> {
        Some(self.summary.message_count.max(0) as u64)
    }
}

/// The `ClaudeHome` for a resolved root. The env-resolved home carries
/// the registry location Claude Code uses for it (`$HOME/.claude.json`
/// as a sibling of `$HOME/.claude`); any other root keeps
/// `.claude.json` inside itself.
fn claude_home_for_root(root: &Path) -> Result<ClaudeHome, io::Error> {
    match crate::home::claude_home() {
        Some(default) if default == root => ClaudeHome::from_env(),
        _ => Ok(ClaudeHome::new(root.to_path_buf())),
    }
}

struct ClaudeStoredSession {
    native_id: String,
    content_format: String,
    source: Box<dyn ContentSource>,
}

impl StoredSession for ClaudeStoredSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }

    fn content_format(&self) -> &str {
        &self.content_format
    }

    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Box<dyn Entry>, DriverError>> + '_> {
        let reader = match self.source.open("session.jsonl") {
            Ok(r) => BufReader::new(r as Box<dyn Read + Send>),
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        Box::new(reader.lines().enumerate().map(|(idx, line)| {
            let raw = line.map_err(DriverError::Io)?;
            Ok(Box::new(ClaudeEntry {
                line: (idx as u32) + 1,
                raw,
            }) as Box<dyn Entry>)
        }))
    }
}

/// One raw line of `session.jsonl`
struct ClaudeEntry {
    line: u32,
    raw: String,
}

impl Entry for ClaudeEntry {
    fn line(&self) -> u32 {
        self.line
    }

    fn raw(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn delete_native_removes_session_and_sidecar() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let path = seed_session(root, "/Users/alice/Code/gage", id, "");
        let sidecar = path.with_extension("");
        fs::create_dir_all(sidecar.join("subagents")).unwrap();

        let source = ClaudeDriver::new().open_source(&source_for(root)).unwrap();
        source.delete_native(id).unwrap();
        assert!(!path.exists());
        assert!(!sidecar.exists());
        assert!(source.open_native(id).is_err());
    }

    #[test]
    fn delete_native_unknown_id_errors() {
        let tmp = TempDir::new().unwrap();
        let source = ClaudeDriver::new()
            .open_source(&source_for(tmp.path()))
            .unwrap();
        let err = source.delete_native("nope").unwrap_err();
        assert!(err.to_string().contains("native session not found"));
    }

    #[test]
    fn format_project_strips_home_prefix() {
        let driver = ClaudeDriver::new();
        assert_eq!(
            driver.format_project("-home-garrett-Code-gage", 25),
            "Code-gage"
        );
        assert_eq!(
            driver.format_project("-Users-alice-Code-gage", 25),
            "Code-gage"
        );
        assert_eq!(driver.format_project("-tmp-scratch", 25), "-tmp-scratch");
        assert_eq!(driver.format_project("-home-garrett", 25), "-home-garrett");
        assert_eq!(
            driver.format_project("-home-garrett-", 25),
            "-home-garrett-"
        );
    }

    #[test]
    fn format_project_splits_head_and_tail_by_quarter() {
        let driver = ClaudeDriver::new();
        let short = "-home-garrett-Code-gage";
        let long = "-home-garrett-Code-gage-crates-gage-cli-src";
        let deep = "-home-garrett-Projects-abcdef-ghijk-7654-90876-hajhasd-1232132";
        assert_eq!(driver.format_project(short, 7), "C…-gage");
        assert_eq!(driver.format_project(long, 7), "C…i-src");
        assert_eq!(driver.format_project(deep, 7), "P…32132");
        assert_eq!(driver.format_project(short, 12), "Code-gage");
        assert_eq!(driver.format_project(long, 12), "Cod…-cli-src");
        assert_eq!(driver.format_project(deep, 12), "Pro…-1232132");
        assert_eq!(driver.format_project(long, 18), "Code…-gage-cli-src");
        assert_eq!(driver.format_project(deep, 18), "Proj…jhasd-1232132");
    }

    #[test]
    fn format_project_tiny_budgets() {
        let driver = ClaudeDriver::new();
        let name = "averylongprojectname";
        assert_eq!(driver.format_project(name, 3), "…me");
        assert_eq!(driver.format_project(name, 2), "…e");
        assert_eq!(driver.format_project(name, 1), "…");
        assert_eq!(driver.format_project(name, 0), "");
    }

    #[test]
    fn format_model_strips_vendor_prefix() {
        let driver = ClaudeDriver::new();
        assert_eq!(driver.format_model("claude-fable-5-1"), "fable-5-1");
        assert_eq!(driver.format_model("gpt-x"), "gpt-x");
        assert_eq!(driver.format_model(""), "");
    }

    fn seed_session(root: &Path, cwd: &str, id: &str, contents: &str) -> PathBuf {
        let slug = encode_project_dir(Path::new(cwd));
        let dir = root.join("projects").join(&slug);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, contents).unwrap();
        path
    }

    fn source_for(root: &Path) -> String {
        format!("{NAME}:{}", root.to_string_lossy())
    }

    #[test]
    fn find_native_disambiguates_prefix() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let a = "aaaaaaaa-1111-1111-1111-111111111111";
        let b = "bbbbbbbb-2222-2222-2222-222222222222";
        seed_session(root, "/Users/alice/x", a, "");
        seed_session(root, "/Users/alice/y", b, "");

        let source = ClaudeDriver::new().open_source(&source_for(root)).unwrap();
        assert_eq!(source.find_native("aaaa").unwrap(), a);
        let bare = ClaudeDriver::new()
            .open_source(&root.to_string_lossy())
            .unwrap();
        assert_eq!(bare.find_native("bbbb").unwrap(), b);
        match ClaudeDriver::new().open_source("zzz:/x") {
            Err(DriverError::Other(msg)) => assert!(msg.contains("zzz"), "{msg}"),
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("unknown scheme was accepted"),
        }
        let err = source.find_native("no-such").unwrap_err();
        assert!(matches!(err, NativeLookupError::NoMatch(_)));
    }

    #[test]
    fn native_session_source_is_root_and_id() {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        seed_session(&root, "/Users/alice/x", id, "");
        let source = ClaudeDriver::new().open_source(&source_for(&root)).unwrap();
        let session = source.open_native(id).unwrap();
        assert_eq!(session.source(), format!("claude:{}/{id}", root.display()));
    }

    #[test]
    fn project_name_encodes_slug() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cwd = "/Users/alice/Code/gage";
        seed_session(root, cwd, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "");

        let source = ClaudeDriver::new().open_source(&source_for(root)).unwrap();
        let name = source.project_name(Path::new(cwd)).unwrap();
        assert_eq!(name, "-Users-alice-Code-gage");
    }

    #[test]
    fn project_path_resolves_through_registry() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cwd = tmp.path().join("work");
        fs::create_dir_all(&cwd).unwrap();
        let cwd = fs::canonicalize(&cwd).unwrap();
        seed_session(
            root,
            &cwd.to_string_lossy(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "",
        );
        fs::write(
            root.join(".claude.json"),
            format!(r#"{{"projects": {{"{}": {{}}}}}}"#, cwd.display()),
        )
        .unwrap();

        let source = ClaudeDriver::new().open_source(&source_for(root)).unwrap();
        let slug = encode_project_dir(&cwd);
        assert_eq!(source.project_path(&slug).unwrap(), Some(cwd));
        assert_eq!(source.project_path("-no-such").unwrap(), None);
    }
}
