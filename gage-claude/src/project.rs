//! Projects Claude Code has recorded for a user.
//!
//! A `Project` is identified by its real on-disk cwd. The encoded
//! `~/.claude/projects/<dir>` name is *not* part of project identity;
//! see [`crate::session::encode_project_dir`] for the encoding and why.

use std::io;
use std::path::PathBuf;

use crate::config::{self, ConfigFiles, ProjectWants};
use crate::home::ClaudeHome;
use crate::session::encode_project_dir;

/// A directory where Claude Code has been used. Carries just the path
/// today; future attributes from `~/.claude.json` (e.g. last-opened
/// timestamp, trust status) can be added without changing call sites.
#[derive(Debug, Clone)]
pub struct Project {
    pub path: PathBuf,
}

impl Project {
    /// Start a finder for project-scope config files rooted at the
    /// project's cwd. Each phase (settings, local settings, memory,
    /// local memory, skills, commands, agents) is enabled by default;
    /// toggle individual phases off to skip the I/O they would do.
    /// Call `.find()` to get the lazy iterator.
    pub fn config(&self) -> ProjectFinder {
        ProjectFinder {
            root: self.path.clone(),
            wants: ProjectWants {
                settings: true,
                local_settings: true,
                memory: true,
                local_memory: true,
                skills: true,
                commands: true,
                agents: true,
            },
        }
    }
}

/// Builder for a project-scope `ConfigFiles` walk. Returned by
/// [`Project::config`]. All phases start enabled; chain
/// `.<phase>(false)` calls to disable individual phases, then call
/// `.find()` to consume the builder.
pub struct ProjectFinder {
    root: PathBuf,
    wants: ProjectWants,
}

impl ProjectFinder {
    pub fn settings(mut self, on: bool) -> Self {
        self.wants.settings = on;
        self
    }
    pub fn local_settings(mut self, on: bool) -> Self {
        self.wants.local_settings = on;
        self
    }
    pub fn memory(mut self, on: bool) -> Self {
        self.wants.memory = on;
        self
    }
    pub fn local_memory(mut self, on: bool) -> Self {
        self.wants.local_memory = on;
        self
    }
    /// Skills *and* their rule files. Both come out of one `read_dir`,
    /// so they share a toggle.
    pub fn skills(mut self, on: bool) -> Self {
        self.wants.skills = on;
        self
    }
    pub fn commands(mut self, on: bool) -> Self {
        self.wants.commands = on;
        self
    }
    pub fn agents(mut self, on: bool) -> Self {
        self.wants.agents = on;
        self
    }

    pub fn find(self) -> ConfigFiles {
        config::project_files(self.root, self.wants)
    }
}

/// Find a project by the encoded directory name a session JSONL lives
/// under. The encoding is lossy and ambiguous, so this returns the
/// *first* project whose path encodes to `name`; if multiple cwds
/// collide onto the same encoded name only one is recovered. Returns
/// `Ok(None)` when no recorded project matches.
pub fn project_for_session_name(home: &ClaudeHome, name: &str) -> io::Result<Option<Project>> {
    let projects = home.projects()?;
    Ok(projects
        .into_iter()
        .find(|p| encode_project_dir(&p.path) == name))
}
