//! Claude Code session JSONL discovery and basic session-file ops.
//!
//! Sessions live under `~/.claude/projects/<encoded>/<uuid>.jsonl`. The
//! encoded directory name is the project's cwd with every non-ASCII-alnum
//! character replaced by `-`; the encoding is lossy and not used as a
//! project identity (see `crate::project`). This module surfaces the
//! sessions themselves; it does not resolve them back to project paths.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use gage_core::config::gage_home;
use regex::Regex;
use serde::{Serialize, Serializer};

pub static SESSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.jsonl$").unwrap()
});

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(skip)]
    pub src: PathBuf,
    #[serde(serialize_with = "serialize_system_time")]
    pub mtime: SystemTime,
    pub size: u64,
}

impl SessionInfo {
    /// The encoded directory name under `~/.claude/projects/` that
    /// holds this session's JSONL. This is *not* a stable project
    /// identity — the encoding is lossy and multiple cwds can collide
    /// onto the same encoded name. Use it only as an index into the
    /// session-storage layout.
    pub fn project_name(&self) -> std::borrow::Cow<'_, str> {
        self.src
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
    }
}

fn serialize_system_time<S: Serializer>(time: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    s.serialize_f64(duration.as_secs_f64())
}

pub struct SessionList {
    sessions: Vec<SessionInfo>,
}

impl SessionList {
    pub fn iter(&self) -> std::slice::Iter<'_, SessionInfo> {
        self.sessions.iter()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

impl IntoIterator for SessionList {
    type Item = SessionInfo;
    type IntoIter = std::vec::IntoIter<SessionInfo>;

    fn into_iter(self) -> Self::IntoIter {
        self.sessions.into_iter()
    }
}

impl<'a> IntoIterator for &'a SessionList {
    type Item = &'a SessionInfo;
    type IntoIter = std::slice::Iter<'a, SessionInfo>;

    fn into_iter(self) -> Self::IntoIter {
        self.sessions.iter()
    }
}

pub struct SessionListBuilder {
    root: Option<PathBuf>,
    projects: Vec<String>,
    since: Option<SystemTime>,
    limit: Option<usize>,
    empty: bool,
}

impl SessionListBuilder {
    pub fn new() -> Self {
        SessionListBuilder {
            root: None,
            projects: Vec::new(),
            since: None,
            limit: None,
            empty: false,
        }
    }

    pub fn root(mut self, path: impl Into<PathBuf>) -> Self {
        self.root = Some(path.into());
        self
    }

    pub fn project(mut self, path: &Path) -> Self {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.projects.push(encode_project_dir(&canonical));
        self
    }

    /// Filter by an already-encoded project directory name, as
    /// recorded in the session table's `project` column.
    pub fn project_slug(mut self, slug: impl Into<String>) -> Self {
        self.projects.push(slug.into());
        self
    }

    pub fn since(mut self, duration: Duration) -> Self {
        self.since = SystemTime::now().checked_sub(duration);
        self
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn empty(mut self, yes: bool) -> Self {
        self.empty = yes;
        self
    }

    pub fn build(self) -> SessionList {
        let projects_dir = self
            .root
            .or_else(projects_dir)
            .expect("CLAUDE_PROJECTS_DIR or HOME must be set");

        let mut sessions = Vec::new();

        let entries = match std::fs::read_dir(&projects_dir) {
            Ok(entries) => entries,
            Err(_) => return SessionList { sessions },
        };

        for project_entry in entries.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }

            if !self.projects.is_empty() {
                let dir_name = project_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !self.projects.contains(&dir_name) {
                    continue;
                }
            }

            let dir_entries = match std::fs::read_dir(&project_path) {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for entry in dir_entries.flatten() {
                let path = entry.path();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                if !SESSION_RE.is_match(&name) {
                    continue;
                }
                let id = &name[..36];

                let metadata = match path.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let size = metadata.len();

                if let Some(cutoff) = self.since
                    && mtime < cutoff
                {
                    continue;
                }

                if self.empty && !is_empty_session(&path) {
                    continue;
                }

                sessions.push(SessionInfo {
                    id: id.to_string(),
                    src: path,
                    mtime,
                    size,
                });
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        if let Some(n) = self.limit {
            sessions.truncate(n);
        }
        SessionList { sessions }
    }
}

impl Default for SessionListBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub fn ls_sessions() -> Vec<(String, PathBuf)> {
    SessionListBuilder::new()
        .build()
        .into_iter()
        .map(|s| (s.id, s.src))
        .collect()
}

/// Root directory holding per-project session subdirectories. The
/// canonical resolver — any caller needing this location uses it
/// rather than hardcoding `.claude/projects`. Honors `CLAUDE_PROJECTS_DIR`
/// (test fixtures, alternate session stores); otherwise
/// `claude_home()/projects`. `None` only when neither override nor
/// `HOME` is set.
pub fn projects_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_PROJECTS_DIR") {
        return Some(PathBuf::from(dir));
    }
    crate::home::claude_home().map(|h| h.join("projects"))
}

/// Locate an agent session JSONL in the gage archive,
/// `<gage_home>/claude/<name>/<session_id>.jsonl`. The sandbox name
/// is not recorded with the session id, so every archive subdirectory
/// is probed. None when the session is not archived.
pub fn find_agent_session(session_id: &str) -> Option<PathBuf> {
    let archive = gage_home().join("claude");
    let file = format!("{session_id}.jsonl");
    let entries = std::fs::read_dir(&archive).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn find_session(id_prefix: &str) -> Vec<SessionInfo> {
    let projects_dir = projects_dir().expect("CLAUDE_PROJECTS_DIR or HOME must be set");

    let mut results = Vec::new();

    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(entries) => entries,
        Err(_) => return results,
    };

    for project_entry in entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let dir_entries = match std::fs::read_dir(&project_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in dir_entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if !name.starts_with(id_prefix) || !SESSION_RE.is_match(&name) {
                continue;
            }
            let id = &name[..36];

            let metadata = match path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let size = metadata.len();

            results.push(SessionInfo {
                id: id.to_string(),
                src: path,
                mtime,
                size,
            });
        }
    }

    results
}

#[derive(Debug)]
pub enum SessionLookupError {
    NoMatch(String),
    TooManyMatches((String, Vec<String>)),
}

impl fmt::Display for SessionLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionLookupError::NoMatch(s) => write!(f, "no sessions match {s}"),
            SessionLookupError::TooManyMatches((s, ids)) => {
                write!(f, "Found more than one session matching {s}")?;
                for id in ids {
                    write!(f, "\n  {id}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SessionLookupError {}

pub fn one_session(prefix: &str) -> Result<SessionInfo, SessionLookupError> {
    let matches = find_session(prefix);
    match matches.len() {
        0 => Err(SessionLookupError::NoMatch(prefix.into())),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            let mut ids: Vec<String> = matches.into_iter().map(|s| s.id).collect();
            ids.sort();
            Err(SessionLookupError::TooManyMatches((prefix.into(), ids)))
        }
    }
}

fn is_empty_session(path: &Path) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return true,
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return true,
        };
        if line.contains("\"type\":\"user\"")
            || line.contains("\"type\": \"user\"")
            || line.contains("\"type\":\"assistant\"")
            || line.contains("\"type\": \"assistant\"")
        {
            return false;
        }
    }
    true
}

/// Delete a session's JSONL file and sidecar directory. Gage notes
/// targeting the session are untouched: note targets are advisory
/// references with no lifetime coupling, so the notes outlive the
/// session by design.
pub fn delete_session(path: &Path) -> io::Result<()> {
    std::fs::remove_file(path)?;
    let sidecar = path.with_extension("");
    if sidecar.is_dir() {
        std::fs::remove_dir_all(sidecar)?;
    }
    Ok(())
}

/// Encode a project cwd into the directory name Claude Code uses under
/// `~/.claude/projects/`. Lossy: every non-ASCII-alnum character (including
/// the path separator) becomes `-`.
pub fn encode_project_dir(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut encoded = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            encoded.push(c);
        } else {
            encoded.push('-');
        }
    }
    encoded
}
