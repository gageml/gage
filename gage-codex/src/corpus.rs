//! The Codex corpus: the rollout files under a Codex home, as the
//! shared derived data layer reads them.

use std::io;
use std::path::{Path, PathBuf};

use gage_corpus::{Corpus, DerivedSession, Result, SessionInfo, project_slug};

use crate::derive::{SUMMARY_VERSION, derive_session};
use crate::driver::{CONTENT_FORMAT, NAME, SESSION_TYPE};
use crate::rollout::{SESSIONS_DIR, SessionMeta, read_meta, rollout_files, thread_name};

/// The sessions under one Codex home.
#[derive(Debug)]
pub(crate) struct CodexCorpus {
    home: PathBuf,
    sessions_dir: PathBuf,
}

impl CodexCorpus {
    pub(crate) fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let sessions_dir = home.join(SESSIONS_DIR);
        Self { home, sessions_dir }
    }
}

/// The project name of a session: the slug of the working directory
/// it started in, as its first record names it. A rollout whose first
/// record names none has no project.
pub(crate) fn project_of(meta: &SessionMeta) -> String {
    meta.cwd.as_deref().map(project_slug).unwrap_or_default()
}

impl Corpus for CodexCorpus {
    fn driver(&self) -> &'static str {
        NAME
    }

    fn session_type(&self) -> &'static str {
        SESSION_TYPE
    }

    fn content_format(&self) -> &'static str {
        CONTENT_FORMAT
    }

    fn summary_version(&self) -> u32 {
        SUMMARY_VERSION
    }

    fn root(&self) -> &Path {
        &self.home
    }

    fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// The rollouts of the threads people started, newest first. A
    /// thread anything else started, a sub-agent or one of Gage's own
    /// scan agents, is listed only with `show_agents`. The first
    /// record of each rollout is read for its thread source and its
    /// working directory; nothing else of a rollout is.
    fn sessions(&self, show_agents: bool) -> io::Result<Vec<SessionInfo>> {
        let mut sessions = Vec::new();
        for file in rollout_files(&self.sessions_dir)? {
            let meta = match read_meta(&file.path) {
                Ok(meta) => meta,
                // Removed since the walk listed it
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if !show_agents && !meta.is_user_thread() {
                continue;
            }
            sessions.push(SessionInfo {
                id: file.id,
                src: file.path,
                mtime: file.mtime,
                size: file.size,
                project: project_of(&meta),
            });
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        Ok(sessions)
    }

    fn derive(&self, id: &str, path: &Path) -> Result<DerivedSession> {
        let project = project_of(&read_meta(path)?);
        derive_session(id, path, project)
    }

    fn recorded_title(&self, id: &str) -> Option<String> {
        thread_name(&self.home, id)
    }
}
