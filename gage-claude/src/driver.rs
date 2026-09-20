//! `ClaudeDriver` implements `gage_session::Driver` for Claude Code
//! sessions stored under a projects directory as `<slug>/<uuid>.jsonl`.
//!
//! Source URL grammar:
//! - `claude:` -- default location (`$CLAUDE_CONFIG_DIR` or `$HOME/.claude`)
//! - `claude:<path>` -- explicit filesystem root; sessions live under
//!   `<path>/projects/**/*.jsonl`

use std::borrow::Cow;
use std::cell::OnceCell;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use std::sync::Arc;

use gage_session::{
    ContentSink, ContentSource, Driver, DriverError, DriverTables, Entry, Message,
    NativeLookupError, NativeSession, Project, ProjectSpec, SessionAttrs, SourceUrl, StoredSession,
};

use crate::home::ClaudeHome;
use crate::index::IndexStore;
use crate::session::{SESSION_RE, encode_project_dir, is_agent_tmp_slug, is_empty_session};
use crate::tables::{EntryTable, MessageTable, SessionTable};

const NAME: &str = "claude";
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

    fn tables(&self, source: &SourceUrl) -> Result<DriverTables, DriverError> {
        let root = resolve_root(source)?;
        let projects_dir = root.join("projects");
        let cache_dir = default_cache_dir(&projects_dir);
        let store = Arc::new(IndexStore::new(projects_dir, cache_dir));
        Ok(DriverTables {
            session: Arc::new(SessionTable::new(Arc::clone(&store))),
            message: Arc::new(MessageTable::new(Arc::clone(&store))),
            entry: Arc::new(EntryTable::new(store)),
        })
    }

    fn find_native(&self, source: &SourceUrl, prefix: &str) -> Result<String, NativeLookupError> {
        let root = resolve_root(source).map_err(NativeLookupError::Driver)?;
        let projects_dir = root.join("projects");
        let mut matches = Vec::new();
        for hit in walk_session_files(&projects_dir) {
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

    fn open_native(
        &self,
        source: &SourceUrl,
        native_id: &str,
    ) -> Result<Box<dyn NativeSession>, DriverError> {
        let root = resolve_root(source)?;
        let projects_dir = root.join("projects");
        for hit in walk_session_files(&projects_dir) {
            let (id, path, meta) = hit?;
            if id == native_id {
                return Ok(Box::new(ClaudeNativeSession::new(id, path, meta, root)));
            }
        }
        Err(DriverError::Other(format!(
            "native session not found: {NAME}:{native_id}"
        )))
    }

    fn project(
        &self,
        source: &SourceUrl,
        spec: ProjectSpec,
    ) -> Result<Option<Box<dyn Project>>, DriverError> {
        let root = resolve_root(source)?;
        match spec {
            ProjectSpec::Path(path) => {
                let canonical = fs::canonicalize(&path).unwrap_or(path);
                let slug = encode_project_dir(&canonical);
                Ok(Some(Box::new(ClaudeProject {
                    slug,
                    path: Some(canonical),
                })))
            }
            ProjectSpec::Name(name) => {
                let home = claude_home_for(source, root)?;
                let path = match home.projects() {
                    Ok(list) => list
                        .into_iter()
                        .find(|p| encode_project_dir(&p.path) == name)
                        .map(|p| p.path),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => return Err(DriverError::Io(e)),
                };
                Ok(Some(Box::new(ClaudeProject { slug: name, path })))
            }
        }
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
        let session_path = claude.session_path().to_path_buf();
        copy_file_into_sink(&session_path, "session.jsonl", sink)?;
        let subagents_dir = subagents_dir_for(&session_path, claude.native_id());
        match fs::read_dir(&subagents_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(DriverError::Io)?;
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| {
                            DriverError::Other(format!(
                                "non-UTF-8 name under {}",
                                subagents_dir.display()
                            ))
                        })?
                        .to_string();
                    copy_file_into_sink(&path, &format!("subagents/{name}"), sink)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(DriverError::Io(e)),
        }
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

fn subagents_dir_for(session_path: &Path, native_id: &str) -> PathBuf {
    session_path
        .parent()
        .expect("session path has a parent")
        .join(native_id)
        .join("subagents")
}

/// Where the index cache lives for a Claude projects directory. Two
/// origins are recognized so the default corpus and the gage-agent
/// corpus never share a summary cache, text index, or reconcile
/// manifest.
fn default_cache_dir(projects_dir: &Path) -> PathBuf {
    let gage_home = gage_core::config::gage_home();
    let agent_projects_dir = gage_home.join("claude");
    let origin = if projects_dir == agent_projects_dir {
        "agent"
    } else {
        "default"
    };
    gage_home.join("cache").join(origin)
}

/// Resolve the filesystem root from a `claude:` source URL. Empty
/// body -> the ambient Claude home. Non-empty body is treated as a
/// path (`~` expanded).
fn resolve_root(source: &SourceUrl) -> Result<PathBuf, DriverError> {
    if source.scheme() != NAME {
        return Err(DriverError::Other(format!(
            "expected scheme {NAME}, got {}",
            source.scheme(),
        )));
    }
    let body = source.body();
    if body.is_empty() {
        return crate::home::claude_home()
            .ok_or_else(|| DriverError::Other("CLAUDE_CONFIG_DIR or HOME must be set".into()));
    }
    Ok(expand_tilde(body))
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

pub struct ClaudeNativeSession {
    native_id: String,
    session_path: PathBuf,
    attrs: ClaudeSessionAttrs,
}

impl ClaudeNativeSession {
    fn new(native_id: String, session_path: PathBuf, meta: fs::Metadata, root: PathBuf) -> Self {
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let size = meta.len();
        let project_slug = session_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let attrs = ClaudeSessionAttrs {
            mtime,
            size,
            project_slug,
            root,
            session_path: session_path.clone(),
            is_empty: OnceLock::new(),
            project_path: OnceCell::new(),
        };
        Self {
            native_id,
            session_path,
            attrs,
        }
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }
}

impl NativeSession for ClaudeNativeSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }

    fn session_type(&self) -> &str {
        SESSION_TYPE
    }

    fn attrs(&self) -> &dyn SessionAttrs {
        &self.attrs
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Cheap-to-hold attribute reader. `mtime`, `size`, and
/// `project_slug` are captured at enumeration time from the directory
/// walk. `is_empty` reads the session file on first access.
/// `project_path` reads `~/.claude.json` on first access. `title`,
/// `model`, `message_count` are not answered here.
struct ClaudeSessionAttrs {
    mtime: SystemTime,
    size: u64,
    project_slug: String,
    root: PathBuf,
    session_path: PathBuf,
    is_empty: OnceLock<bool>,
    project_path: OnceCell<Option<PathBuf>>,
}

impl SessionAttrs for ClaudeSessionAttrs {
    fn mtime(&self) -> Option<SystemTime> {
        Some(self.mtime)
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }

    fn is_empty(&self) -> Option<bool> {
        Some(
            *self
                .is_empty
                .get_or_init(|| is_empty_session(&self.session_path).unwrap_or(true)),
        )
    }

    fn project_name(&self) -> Option<&str> {
        Some(&self.project_slug)
    }

    fn project_path(&self) -> Option<&Path> {
        let cell = self
            .project_path
            .get_or_init(|| resolve_project_path(&self.root, &self.project_slug));
        cell.as_deref()
    }
}

fn resolve_project_path(root: &Path, slug: &str) -> Option<PathBuf> {
    let home = claude_home_for_root(root).ok()?;
    let projects = home.projects().ok()?;
    projects
        .into_iter()
        .find(|p| encode_project_dir(&p.path) == slug)
        .map(|p| p.path)
}

/// Choose a `ClaudeHome` for the source. Empty body means the default
/// location, which reads `.claude.json` as a sibling of `.claude/`.
/// A non-empty body is treated as a self-contained Claude root (test
/// fixtures and future user layouts put `.claude.json` inside).
fn claude_home_for(source: &SourceUrl, root: PathBuf) -> Result<ClaudeHome, DriverError> {
    if source.body().is_empty() {
        ClaudeHome::from_env().map_err(DriverError::Io)
    } else {
        Ok(ClaudeHome::new(root))
    }
}

fn claude_home_for_root(root: &Path) -> Result<ClaudeHome, io::Error> {
    match crate::home::claude_home() {
        Some(default) if default == root => ClaudeHome::from_env(),
        _ => Ok(ClaudeHome::new(root.to_path_buf())),
    }
}

pub struct ClaudeProject {
    slug: String,
    path: Option<PathBuf>,
}

impl Project for ClaudeProject {
    fn name(&self) -> &str {
        &self.slug
    }

    fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn is_for(&self, session: &dyn NativeSession) -> bool {
        session
            .attrs()
            .project_name()
            .is_some_and(|n| n == self.slug)
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

/// One entry row read from `session.jsonl`. Rich fields (`type_`,
/// `subtype`, `uuid`, `timestamp`, `to_message`) are stubbed today;
/// `raw` is the source line and drives every current consumer.
struct ClaudeEntry {
    line: u32,
    raw: String,
}

impl Entry for ClaudeEntry {
    fn line(&self) -> u32 {
        self.line
    }

    fn uuid(&self) -> Option<&str> {
        None
    }

    fn timestamp(&self) -> Option<SystemTime> {
        None
    }

    fn raw(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.raw)
    }

    fn type_(&self) -> &str {
        ""
    }

    fn subtype(&self) -> Option<&str> {
        None
    }

    fn to_message(&self) -> Option<&dyn Message> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seed_session(root: &Path, cwd: &str, id: &str, contents: &str) -> PathBuf {
        let slug = encode_project_dir(Path::new(cwd));
        let dir = root.join("projects").join(&slug);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, contents).unwrap();
        path
    }

    fn source_for(root: &Path) -> SourceUrl {
        SourceUrl::new(NAME, root.to_string_lossy().into_owned())
    }

    #[test]
    fn find_native_disambiguates_prefix() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let a = "aaaaaaaa-1111-1111-1111-111111111111";
        let b = "bbbbbbbb-2222-2222-2222-222222222222";
        seed_session(root, "/Users/alice/x", a, "");
        seed_session(root, "/Users/alice/y", b, "");

        let driver = ClaudeDriver::new();
        let source = source_for(root);
        assert_eq!(driver.find_native(&source, "aaaa").unwrap(), a);
        let err = driver.find_native(&source, "no-such").unwrap_err();
        assert!(matches!(err, NativeLookupError::NoMatch(_)));
    }

    #[test]
    fn project_from_path_encodes_slug() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let cwd = "/Users/alice/Code/gage";
        seed_session(root, cwd, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "");

        let driver = ClaudeDriver::new();
        let source = source_for(root);
        let project = driver
            .project(&source, ProjectSpec::Path(PathBuf::from(cwd)))
            .unwrap()
            .unwrap();
        assert_eq!(project.name(), "-Users-alice-Code-gage");
    }
}
