//! `ClaudeDriver`: `gage-session::Driver` for Claude Code sessions.
//!
//! Given `<uuid>`, scans `$CLAUDE_CONFIG_DIR/projects/*` (or the default
//! `~/.claude/projects/*`) for `<uuid>.jsonl` and returns a
//! [`SourceSession`] that streams the transcript plus the adjacent
//! subagent sidecar directory.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

use gage_session::{
    ContentAccess, Driver, DriverError, Entry, SessionFile, SessionType, SourceSession,
    StoreSession,
};

use crate::session::find_session;

const NAME: &str = "claude";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const SESSION_TYPE: &str = "claude";
const SESSION_TYPE_VERSION: &str = "1";

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

    fn resolve(&self, id: &str) -> Result<Box<dyn SourceSession>, DriverError> {
        let matches = find_session(id);
        let hit = matches
            .into_iter()
            .find(|info| info.id == id)
            .ok_or_else(|| DriverError::SessionNotFound(format!("{NAME}:{id}")))?;
        Ok(Box::new(ClaudeSourceSession {
            session_id: hit.id,
            session_type: SessionType::new(SESSION_TYPE, SESSION_TYPE_VERSION),
            session_path: hit.src,
        }))
    }

    fn open(
        &self,
        session_id: String,
        session_type: SessionType,
        _content_format: Option<String>,
        access: Box<dyn ContentAccess>,
    ) -> Result<Box<dyn StoreSession>, DriverError> {
        Ok(Box::new(ClaudeStoreSession {
            session_id,
            session_type,
            access,
        }))
    }
}

struct ClaudeStoreSession {
    session_id: String,
    session_type: SessionType,
    access: Box<dyn ContentAccess>,
}

impl StoreSession for ClaudeStoreSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn session_type(&self) -> &SessionType {
        &self.session_type
    }

    fn content_format(&self) -> Option<&str> {
        None
    }

    fn entries(&mut self) -> Box<dyn Iterator<Item = Result<Entry, DriverError>> + '_> {
        let reader = match self.access.open("session.jsonl") {
            Ok(r) => BufReader::new(r),
            Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
        };
        Box::new(reader.lines().enumerate().map(|(idx, line)| {
            let raw = line.map_err(DriverError::Io)?;
            Ok(Entry {
                line: (idx as u32) + 1,
                raw,
            })
        }))
    }
}

struct ClaudeSourceSession {
    session_id: String,
    session_type: SessionType,
    /// The primary transcript file, e.g.
    /// `~/.claude/projects/<slug>/<uuid>.jsonl`.
    session_path: PathBuf,
}

impl ClaudeSourceSession {
    /// Sidecar directory for subagent files:
    /// `<parent>/<session_id>/subagents/`.
    fn subagents_dir(&self) -> PathBuf {
        self.session_path
            .parent()
            .expect("session path has a parent")
            .join(&self.session_id)
            .join("subagents")
    }
}

impl SourceSession for ClaudeSourceSession {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn session_type(&self) -> &SessionType {
        &self.session_type
    }

    fn content_format(&self) -> Option<&str> {
        None
    }

    fn files(&mut self) -> Box<dyn Iterator<Item = Result<SessionFile, DriverError>> + '_> {
        let session = std::iter::once(open_file(&self.session_path, "session.jsonl".to_string()));
        let subagents_dir = self.subagents_dir();
        let subagents: Box<dyn Iterator<Item = Result<SessionFile, DriverError>>> =
            match std::fs::read_dir(&subagents_dir) {
                Ok(entries) => Box::new(entries.map(move |entry| {
                    let entry = entry?;
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| {
                            DriverError::Other(format!("non-UTF-8 name under {}", path.display()))
                        })?
                        .to_string();
                    open_file(&path, format!("subagents/{name}"))
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Box::new(std::iter::empty()),
                Err(e) => Box::new(std::iter::once(Err(DriverError::Io(e)))),
            };
        Box::new(session.chain(subagents))
    }
}

fn open_file(path: &std::path::Path, rel: String) -> Result<SessionFile, DriverError> {
    let file = File::open(path)?;
    Ok(SessionFile {
        path: rel,
        content: Box::new(file) as Box<dyn Read + Send>,
    })
}
