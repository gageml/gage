//! The Claude corpus: the session files under a projects directory,
//! as the shared derived data layer reads them.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gage_corpus::{Corpus, DerivedSession, IndexStore, Result, SessionInfo};

use super::derive::{SUMMARY_VERSION, derive_session};
use crate::driver::{CONTENT_FORMAT, NAME, SESSION_TYPE};
use crate::session::SessionListBuilder;

/// The sessions under one Claude projects directory. The harness root
/// the corpus is named by is the directory the projects directory
/// sits in.
#[derive(Debug)]
pub struct ClaudeCorpus {
    projects_dir: PathBuf,
    root: PathBuf,
}

impl ClaudeCorpus {
    pub fn new(projects_dir: impl Into<PathBuf>) -> Self {
        let projects_dir = projects_dir.into();
        let root = projects_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| projects_dir.clone());
        Self { projects_dir, root }
    }
}

impl Corpus for ClaudeCorpus {
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
        &self.root
    }

    fn sessions_dir(&self) -> &Path {
        &self.projects_dir
    }

    fn sessions(&self, show_agents: bool) -> io::Result<Vec<SessionInfo>> {
        Ok(SessionListBuilder::new()
            .root(&self.projects_dir)
            .show_agents(show_agents)
            .build()
            .into_iter()
            .collect())
    }

    fn derive(&self, id: &str, path: &Path) -> Result<DerivedSession> {
        derive_session(id, path)
    }
}

/// The store of derived artifacts for the Claude corpus under
/// `projects_dir`, kept under `cache_root`.
pub fn index_store(projects_dir: impl Into<PathBuf>, cache_root: impl Into<PathBuf>) -> IndexStore {
    IndexStore::new(Arc::new(ClaudeCorpus::new(projects_dir)), cache_root)
}
