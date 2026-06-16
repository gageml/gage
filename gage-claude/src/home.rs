//! Handle to a Claude Code state root.
//!
//! `ClaudeHome` points at the Claude Code state directory: `$CLAUDE_CONFIG_DIR`
//! when set, otherwise `$HOME/.claude`. It is the canonical resolver for
//! every path under that tree (`projects/`, `settings.json`, …) and for
//! the adjacent `.claude.json` registry. Callers needing any of those
//! locations route through here rather than re-deriving them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{debug, trace};

use crate::config::{self, ConfigFiles, UserWants};
use crate::project::Project;
use crate::session::encode_project_dir;

/// The Claude Code state directory: `$CLAUDE_CONFIG_DIR` if set,
/// otherwise `$HOME/.claude`. `None` only when neither env var is set —
/// for us, "$HOME unset" is unrecoverable, so callers may `.unwrap()`.
pub fn claude_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude"))
}

/// Path to the Claude Code project registry (`.claude.json`). Lives at
/// `$CLAUDE_CONFIG_DIR/.claude.json` if set, otherwise `$HOME/.claude.json`
/// (sibling of `$HOME/.claude/`). The file may or may not exist; this
/// only resolves the path.
pub fn claude_json() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join(".claude.json"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude.json"))
}

#[derive(Debug, Clone)]
pub struct ClaudeHome {
    path: PathBuf,
}

impl ClaudeHome {
    /// Build a `ClaudeHome` from the ambient environment via
    /// [`claude_home`]. Errors only when neither `CLAUDE_CONFIG_DIR`
    /// nor `HOME` is set.
    pub fn from_env() -> io::Result<Self> {
        let path =
            claude_home().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
        Ok(Self { path })
    }

    /// Build a `ClaudeHome` rooted at an explicit claude-home directory
    /// (the dir that contains `projects/`, `settings.json`, …). Used by
    /// tests and any caller pointing at a non-default location. The
    /// `.claude.json` registry is read from `path.join(".claude.json")`
    /// for this constructor — fixtures lay it out that way.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// List every project Claude Code has recorded for this home. Reads
    /// `path()/.claude.json` and returns one `Project` per `projects`
    /// key that:
    ///
    /// - still exists as a directory on disk (Claude Code does not
    ///   prune `.claude.json`, so dead cwds are routine), and
    /// - has a corresponding session directory under
    ///   `path()/projects/<encoded>/` (no recorded sessions means there
    ///   is nothing useful to report on the project).
    pub fn projects(&self) -> io::Result<Vec<Project>> {
        let claude_json = self.path.join(".claude.json");
        let text = fs::read_to_string(&claude_json)?;
        debug!(path = %claude_json.display(), bytes = text.len(), "read_to_string: claude.json");
        let parsed: ClaudeJson = serde_json::from_str(&text).map_err(io::Error::other)?;
        let sessions_root = self.path.join("projects");
        let raw_count = parsed.projects.len();
        let kept: Vec<Project> = parsed
            .projects
            .into_keys()
            .filter(|path| {
                // A project path equal to the Claude home root is by
                // definition not a project — it's the directory the
                // Claude state lives under. Treating it as one would
                // walk the entire home tree.
                let ok = path != &self.path;
                if !ok {
                    trace!(path = %path.display(), "projects: drop (equals claude home)");
                }
                ok
            })
            .filter(|path| {
                let ok = path.is_dir();
                if !ok {
                    trace!(path = %path.display(), "projects: drop (cwd missing)");
                }
                ok
            })
            .filter(|path| {
                let encoded = encode_project_dir(path);
                let sessions_dir = sessions_root.join(&encoded);
                let ok = sessions_dir.is_dir();
                if !ok {
                    trace!(path = %path.display(), encoded = %encoded, "projects: drop (no sessions dir)");
                }
                ok
            })
            .map(|path| Project { path })
            .collect();
        debug!(
            raw = raw_count,
            kept = kept.len(),
            "projects: filter result"
        );
        Ok(kept)
    }

    /// Start a finder for user-scope config files under `path()`. Each
    /// phase (settings, root `CLAUDE.md`, skills, commands, agents,
    /// installed plugins) is enabled by default; toggle individual
    /// phases off to skip the I/O they would do. Call `.find()` to get
    /// the lazy iterator.
    pub fn config(&self) -> ClaudeHomeFinder {
        ClaudeHomeFinder {
            home: self.path.clone(),
            wants: UserWants {
                settings: true,
                memory: true,
                skills: true,
                commands: true,
                agents: true,
                plugins: true,
            },
        }
    }
}

/// Builder for a user-scope `ConfigFiles` walk. Returned by
/// [`ClaudeHome::config`]. All phases start enabled; chain
/// `.<phase>(false)` calls to disable individual phases, then call
/// `.find()` to consume the builder.
pub struct ClaudeHomeFinder {
    home: PathBuf,
    wants: UserWants,
}

impl ClaudeHomeFinder {
    pub fn settings(mut self, on: bool) -> Self {
        self.wants.settings = on;
        self
    }
    pub fn memory(mut self, on: bool) -> Self {
        self.wants.memory = on;
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
    /// The installed-plugins index file *and* every per-plugin walk it
    /// drives. One toggle for both, since reading the index without
    /// walking it is the only thing the bundling forecloses.
    pub fn plugins(mut self, on: bool) -> Self {
        self.wants.plugins = on;
        self
    }

    pub fn find(self) -> ConfigFiles {
        config::user_files(self.home, self.wants)
    }
}

#[derive(Deserialize)]
struct ClaudeJson {
    #[serde(default)]
    projects: BTreeMap<PathBuf, serde::de::IgnoredAny>,
}
