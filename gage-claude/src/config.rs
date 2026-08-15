//! Enumerate Claude Code config files lazily.
//!
//! `ConfigFile` describes one discovered file by what kind it is plus
//! the identity tokens that can't be recovered from the path alone
//! (skill name, plugin id, parent-skill for a rule). The public entry
//! points are [`crate::home::ClaudeHome::config`] and
//! [`crate::project::Project::config`]; both return a finder that lets
//! the caller toggle individual phases off before calling `.find()` to
//! get the lazy iterator.
//!
//! The SQL-shaped strings (the `type` column values, the composed
//! `name` strings like `rust::clippy`) live in `gage-query`, not here.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use tracing::{debug, trace};

/// One discovered config file.
///
/// The variant identifies what kind of file it is and carries the
/// identity tokens that are not recoverable from the path alone (skill
/// name, plugin id, parent-skill for a rule). `name` fields are the
/// bare on-disk artifact name (`rust`, `clippy`, `session-review`);
/// composition into wire-format identifiers happens in `gage-query`.
#[derive(Debug, Clone)]
pub enum ConfigFile {
    Settings(PathBuf),
    LocalSettings(PathBuf),

    /// `<root>/CLAUDE.md` for the source scope: `~/.claude/CLAUDE.md`
    /// at user scope, `<project>/CLAUDE.md` at project scope.
    Memory(PathBuf),
    /// `<project>/CLAUDE.local.md`. Only emitted by the project walk.
    LocalMemory(PathBuf),
    /// Root-level stack manifest (`Cargo.toml`, `package.json`, …).
    /// Only emitted by the project walk.
    Manifest(PathBuf),

    Skill {
        name: String,
        path: PathBuf,
    },
    SkillRule {
        skill: String,
        name: String,
        path: PathBuf,
    },
    Command {
        name: String,
        path: PathBuf,
    },
    Agent {
        name: String,
        path: PathBuf,
    },

    InstalledPlugins(PathBuf),
    PluginSkill {
        plugin: String,
        name: String,
        path: PathBuf,
    },
    PluginSkillRule {
        plugin: String,
        skill: String,
        name: String,
        path: PathBuf,
    },
    PluginCommand {
        plugin: String,
        name: String,
        path: PathBuf,
    },
    PluginAgent {
        plugin: String,
        name: String,
        path: PathBuf,
    },
    PluginHook {
        plugin: String,
        name: String,
        path: PathBuf,
    },
    PluginMcp {
        plugin: String,
        name: String,
        path: PathBuf,
    },
    PluginLsp {
        plugin: String,
        name: String,
        path: PathBuf,
    },
    PluginMonitor {
        plugin: String,
        name: String,
        path: PathBuf,
    },
}

impl ConfigFile {
    pub fn path(&self) -> &Path {
        match self {
            ConfigFile::Settings(p)
            | ConfigFile::LocalSettings(p)
            | ConfigFile::Memory(p)
            | ConfigFile::LocalMemory(p)
            | ConfigFile::Manifest(p)
            | ConfigFile::InstalledPlugins(p) => p,
            ConfigFile::Skill { path, .. }
            | ConfigFile::SkillRule { path, .. }
            | ConfigFile::Command { path, .. }
            | ConfigFile::Agent { path, .. }
            | ConfigFile::PluginSkill { path, .. }
            | ConfigFile::PluginSkillRule { path, .. }
            | ConfigFile::PluginCommand { path, .. }
            | ConfigFile::PluginAgent { path, .. }
            | ConfigFile::PluginHook { path, .. }
            | ConfigFile::PluginMcp { path, .. }
            | ConfigFile::PluginLsp { path, .. }
            | ConfigFile::PluginMonitor { path, .. } => path,
        }
    }

    /// Modification time as epoch milliseconds.
    pub fn modified(&self) -> io::Result<i64> {
        let modified = fs::metadata(self.path())?.modified()?;
        let ms = modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(ms)
    }

    /// File length in bytes. Mirrors `std::fs::Metadata::len`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> io::Result<u64> {
        Ok(fs::metadata(self.path())?.len())
    }

    pub fn read_to_string(&self) -> io::Result<String> {
        fs::read_to_string(self.path())
    }
}

/// Snapshot of the filesystem ops performed by a `ConfigFiles` walk so
/// far. `files_read` counts every `read_to_string` *inside* the walk
/// (the plugin index, per-plugin manifests). `dirs_listed` counts every
/// `read_dir` the walker performs.
#[derive(Debug, Default, Clone, Copy)]
pub struct WalkMetrics {
    pub files_read: u64,
    pub dirs_listed: u64,
}

#[derive(Default)]
struct Counters {
    files_read: AtomicU64,
    dirs_listed: AtomicU64,
}

impl Counters {
    fn file_read(&self) {
        self.files_read.fetch_add(1, Ordering::Relaxed);
    }
    fn dir_listed(&self) {
        self.dirs_listed.fetch_add(1, Ordering::Relaxed);
    }
    fn snapshot(&self) -> WalkMetrics {
        WalkMetrics {
            files_read: self.files_read.load(Ordering::Relaxed),
            dirs_listed: self.dirs_listed.load(Ordering::Relaxed),
        }
    }
}

/// Lazy iterator over config files, with running metrics. Each `next()`
/// pulls the next file from the chained walk; `metrics()` snapshots
/// what the walk has cost so far.
pub struct ConfigFiles {
    inner: ConfigFileIter,
    counters: Arc<Counters>,
}

impl Iterator for ConfigFiles {
    type Item = io::Result<ConfigFile>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl ConfigFiles {
    /// Snapshot of filesystem ops performed up to this point.
    pub fn metrics(&self) -> WalkMetrics {
        self.counters.snapshot()
    }
}

/// Type-erased iterator alias used to chain phases internally.
type ConfigFileIter = Box<dyn Iterator<Item = io::Result<ConfigFile>>>;

/// Selection of which user-scope phases the walk should run.
pub(crate) struct UserWants {
    pub settings: bool,
    pub memory: bool,
    pub skills: bool,
    pub commands: bool,
    pub agents: bool,
    pub plugins: bool,
}

/// Selection of which project-scope phases the walk should run.
pub(crate) struct ProjectWants {
    pub settings: bool,
    pub local_settings: bool,
    pub memory: bool,
    pub local_memory: bool,
    pub manifest: bool,
    pub skills: bool,
    pub commands: bool,
    pub agents: bool,
}

/// Walk user-scope config rooted at `home` (the parent that contains
/// `.claude/` and `.claude.json`). Each `wants.*` flag gates one phase
/// of the walk; a `false` flag elides every I/O the phase would do.
pub(crate) fn user_files(claude_home: PathBuf, wants: UserWants) -> ConfigFiles {
    debug!(
        home = %claude_home.display(),
        settings = wants.settings,
        memory = wants.memory,
        skills = wants.skills,
        commands = wants.commands,
        agents = wants.agents,
        plugins = wants.plugins,
        "user_files walk",
    );
    let counters = Arc::new(Counters::default());
    let claude = claude_home;
    let inner: ConfigFileIter = Box::new(
        empty_iter()
            .chain(opt(
                wants.settings,
                check_file(claude.join("settings.json"), ConfigFile::Settings),
            ))
            .chain(opt(
                wants.memory,
                check_file(claude.join("CLAUDE.md"), ConfigFile::Memory),
            ))
            .chain(opt(
                wants.skills,
                walk_skills(claude.join("skills"), SkillKind::User, counters.clone()),
            ))
            .chain(opt(
                wants.commands,
                walk_named_md(
                    claude.join("commands"),
                    NamedMdKind::Command,
                    counters.clone(),
                ),
            ))
            .chain(opt(
                wants.agents,
                walk_named_md(claude.join("agents"), NamedMdKind::Agent, counters.clone()),
            ))
            .chain(opt(
                wants.plugins,
                walk_plugins_phase(
                    claude.join("plugins").join("installed_plugins.json"),
                    counters.clone(),
                ),
            )),
    );
    ConfigFiles { inner, counters }
}

/// Walk project-scope config rooted at the project's cwd. Each `wants.*`
/// flag gates one phase; a `false` flag elides every I/O the phase
/// would do (notably `memory(false)` skips the entire recursive walk).
pub(crate) fn project_files(root: PathBuf, wants: ProjectWants) -> ConfigFiles {
    debug!(
        root = %root.display(),
        settings = wants.settings,
        local_settings = wants.local_settings,
        memory = wants.memory,
        local_memory = wants.local_memory,
        manifest = wants.manifest,
        skills = wants.skills,
        commands = wants.commands,
        agents = wants.agents,
        "project_files walk",
    );
    let counters = Arc::new(Counters::default());
    let claude = root.join(".claude");
    let inner: ConfigFileIter = Box::new(
        empty_iter()
            .chain(opt(
                wants.settings,
                check_file(claude.join("settings.json"), ConfigFile::Settings),
            ))
            .chain(opt(
                wants.local_settings,
                check_file(
                    claude.join("settings.local.json"),
                    ConfigFile::LocalSettings,
                ),
            ))
            .chain(opt(
                wants.memory,
                check_file(root.join("CLAUDE.md"), ConfigFile::Memory),
            ))
            .chain(opt(
                wants.local_memory,
                check_file(root.join("CLAUDE.local.md"), ConfigFile::LocalMemory),
            ))
            .chain(opt(wants.manifest, manifest_files(&root)))
            .chain(opt(
                wants.skills,
                walk_skills(claude.join("skills"), SkillKind::User, counters.clone()),
            ))
            .chain(opt(
                wants.commands,
                walk_named_md(
                    claude.join("commands"),
                    NamedMdKind::Command,
                    counters.clone(),
                ),
            ))
            .chain(opt(
                wants.agents,
                walk_named_md(claude.join("agents"), NamedMdKind::Agent, counters.clone()),
            )),
    );
    ConfigFiles { inner, counters }
}

/// Root-level stack manifest files the `manifest` phase probes for.
const MANIFEST_FILES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "CMakeLists.txt",
    "composer.json",
    "mix.exs",
];

fn manifest_files(root: &Path) -> ConfigFileIter {
    let mut iter = empty_iter();
    for name in MANIFEST_FILES {
        iter = Box::new(iter.chain(check_file(root.join(name), ConfigFile::Manifest)));
    }
    iter
}

fn empty_iter() -> ConfigFileIter {
    Box::new(iter::empty())
}

/// Pass `iter` through when `on` is true, else swap for an empty
/// iterator. Used so the chain construction in `user_files` /
/// `project_files` stays one straight `.chain` per phase.
fn opt(on: bool, iter: ConfigFileIter) -> ConfigFileIter {
    if on { iter } else { empty_iter() }
}

fn check_file(path: PathBuf, make: impl FnOnce(PathBuf) -> ConfigFile + 'static) -> ConfigFileIter {
    // Defer the metadata stat until the iterator is first polled, so
    // chain construction stays I/O-free. Stats are not counted as
    // file reads — see `WalkMetrics` doc.
    Box::new(
        iter::once_with(move || match fs::metadata(&path) {
            Ok(m) if m.is_file() => Some(Ok(make(path))),
            Ok(_) => None,
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => Some(Err(e)),
        })
        .flatten(),
    )
}

fn read_dir_iter(
    dir: PathBuf,
    counters: Arc<Counters>,
) -> Box<dyn Iterator<Item = io::Result<fs::DirEntry>>> {
    // `iter::once_with` so the readdir happens on first poll, not at
    // construction time.
    Box::new(
        iter::once_with(
            move || -> Box<dyn Iterator<Item = io::Result<fs::DirEntry>>> {
                match fs::read_dir(&dir) {
                    Ok(rd) => {
                        counters.dir_listed();
                        debug!(path = %dir.display(), "read_dir");
                        Box::new(rd)
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        trace!(path = %dir.display(), "read_dir: not found");
                        Box::new(iter::empty())
                    }
                    Err(e) => {
                        debug!(path = %dir.display(), error = %e, "read_dir failed");
                        Box::new(iter::once(Err(e)))
                    }
                }
            },
        )
        .flatten(),
    )
}

#[derive(Clone)]
enum NamedMdKind {
    Command,
    Agent,
    PluginCommand(String),
    PluginAgent(String),
}

impl NamedMdKind {
    fn build(&self, name: String, path: PathBuf) -> ConfigFile {
        match self {
            NamedMdKind::Command => ConfigFile::Command { name, path },
            NamedMdKind::Agent => ConfigFile::Agent { name, path },
            NamedMdKind::PluginCommand(plugin) => ConfigFile::PluginCommand {
                plugin: plugin.clone(),
                name,
                path,
            },
            NamedMdKind::PluginAgent(plugin) => ConfigFile::PluginAgent {
                plugin: plugin.clone(),
                name,
                path,
            },
        }
    }
}

fn walk_named_md(dir: PathBuf, kind: NamedMdKind, counters: Arc<Counters>) -> ConfigFileIter {
    Box::new(read_dir_iter(dir, counters).filter_map(move |entry| {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Some(Err(e)),
        };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("md")) {
            return None;
        }
        let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
        Some(Ok(kind.build(stem, path)))
    }))
}

#[derive(Clone)]
enum SkillKind {
    User,
    Plugin(String),
}

impl SkillKind {
    fn skill(&self, name: String, path: PathBuf) -> ConfigFile {
        match self {
            SkillKind::User => ConfigFile::Skill { name, path },
            SkillKind::Plugin(plugin) => ConfigFile::PluginSkill {
                plugin: plugin.clone(),
                name,
                path,
            },
        }
    }

    fn rule(&self, skill: String, name: String, path: PathBuf) -> ConfigFile {
        match self {
            SkillKind::User => ConfigFile::SkillRule { skill, name, path },
            SkillKind::Plugin(plugin) => ConfigFile::PluginSkillRule {
                plugin: plugin.clone(),
                skill,
                name,
                path,
            },
        }
    }
}

fn walk_skills(skills_dir: PathBuf, kind: SkillKind, counters: Arc<Counters>) -> ConfigFileIter {
    let inner_counters = counters.clone();
    Box::new(
        read_dir_iter(skills_dir, counters).flat_map(move |entry| -> ConfigFileIter {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return Box::new(iter::once(Err(e))),
            };
            let dir = entry.path();
            if !dir.is_dir() {
                return Box::new(iter::empty());
            }
            let Ok(skill_name) = entry.file_name().into_string() else {
                return Box::new(iter::empty());
            };

            let head: ConfigFileIter = {
                let skill_md = dir.join("SKILL.md");
                let kind = kind.clone();
                let name = skill_name.clone();
                Box::new(
                    iter::once_with(move || match fs::metadata(&skill_md) {
                        Ok(m) if m.is_file() => Some(Ok(kind.skill(name, skill_md))),
                        Ok(_) => None,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                        Err(e) => Some(Err(e)),
                    })
                    .flatten(),
                )
            };

            let rules: ConfigFileIter = {
                let rules_dir = dir.join("rules");
                let kind = kind.clone();
                let skill = skill_name.clone();
                Box::new(read_dir_iter(rules_dir, inner_counters.clone()).filter_map(
                    move |entry| {
                        let entry = match entry {
                            Ok(e) => e,
                            Err(e) => return Some(Err(e)),
                        };
                        let path = entry.path();
                        if path.extension() != Some(OsStr::new("md")) {
                            return None;
                        }
                        let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
                        Some(Ok(kind.rule(skill.clone(), stem, path)))
                    },
                ))
            };

            Box::new(head.chain(rules))
        }),
    )
}

#[derive(Deserialize)]
struct InstalledPluginsFile {
    #[serde(default)]
    plugins: BTreeMap<String, Vec<InstalledPluginEntry>>,
}

#[derive(Deserialize)]
struct InstalledPluginEntry {
    #[serde(rename = "installPath")]
    install_path: PathBuf,
}

#[derive(Deserialize)]
struct PluginManifest {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
}

fn walk_plugins_phase(installed_path: PathBuf, counters: Arc<Counters>) -> ConfigFileIter {
    Box::new(
        iter::once_with(move || -> ConfigFileIter {
            // Skip the whole phase silently if the index file is absent
            // (a user with no plugins installed).
            match fs::metadata(&installed_path) {
                Ok(m) if m.is_file() => {}
                Ok(_) => return Box::new(iter::empty()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Box::new(iter::empty());
                }
                Err(e) => return Box::new(iter::once(Err(e))),
            }

            let text = match fs::read_to_string(&installed_path) {
                Ok(t) => {
                    counters.file_read();
                    debug!(path = %installed_path.display(), bytes = t.len(), "read_to_string: installed plugins index");
                    t
                }
                Err(e) => return Box::new(iter::once(Err(e))),
            };
            let parsed: InstalledPluginsFile = match serde_json::from_str(&text) {
                Ok(p) => p,
                Err(e) => return Box::new(iter::once(Err(io::Error::other(e)))),
            };

            let header = iter::once(Ok(ConfigFile::InstalledPlugins(installed_path)));
            let bodies = parsed
                .plugins
                .into_iter()
                .flat_map(|(plugin_id, entries)| {
                    entries
                        .into_iter()
                        .map(move |entry| (plugin_id.clone(), entry.install_path))
                })
                .flat_map(move |(plugin_id, root)| {
                    walk_plugin_tree(plugin_id, root, counters.clone())
                });
            Box::new(header.chain(bodies))
        })
        .flatten(),
    )
}

fn walk_plugin_tree(plugin_id: String, root: PathBuf, counters: Arc<Counters>) -> ConfigFileIter {
    let skills = walk_skills(
        root.join("skills"),
        SkillKind::Plugin(plugin_id.clone()),
        counters.clone(),
    );
    let commands = walk_named_md(
        root.join("commands"),
        NamedMdKind::PluginCommand(plugin_id.clone()),
        counters.clone(),
    );
    let agents = walk_named_md(
        root.join("agents"),
        NamedMdKind::PluginAgent(plugin_id.clone()),
        counters.clone(),
    );
    let hooks = walk_plugin_files_in_dir(
        root.join("hooks"),
        PluginFileKind::Hook(plugin_id.clone()),
        counters.clone(),
    );
    let monitors = walk_plugin_files_in_dir(
        root.join("monitors"),
        PluginFileKind::Monitor(plugin_id.clone()),
        counters.clone(),
    );
    let servers = walk_plugin_servers(root, plugin_id, counters);

    Box::new(
        skills
            .chain(commands)
            .chain(agents)
            .chain(hooks)
            .chain(monitors)
            .chain(servers),
    )
}

#[derive(Clone)]
enum PluginFileKind {
    Hook(String),
    Monitor(String),
}

impl PluginFileKind {
    fn build(&self, name: String, path: PathBuf) -> ConfigFile {
        match self {
            PluginFileKind::Hook(plugin) => ConfigFile::PluginHook {
                plugin: plugin.clone(),
                name,
                path,
            },
            PluginFileKind::Monitor(plugin) => ConfigFile::PluginMonitor {
                plugin: plugin.clone(),
                name,
                path,
            },
        }
    }
}

fn walk_plugin_files_in_dir(
    dir: PathBuf,
    kind: PluginFileKind,
    counters: Arc<Counters>,
) -> ConfigFileIter {
    Box::new(read_dir_iter(dir, counters).filter_map(move |entry| {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return Some(Err(e)),
        };
        let path = entry.path();
        if !path.is_file() {
            return None;
        }
        let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
        Some(Ok(kind.build(stem, path)))
    }))
}

/// Parse `<plugin>/.claude-plugin/plugin.json` and emit a `PluginMcp`
/// row for each entry in its `mcpServers` map. `PluginLsp` is handled
/// the same way once the manifest field is finalized; until then,
/// nothing is emitted for LSP.
fn walk_plugin_servers(root: PathBuf, plugin: String, counters: Arc<Counters>) -> ConfigFileIter {
    Box::new(
        iter::once_with(move || -> ConfigFileIter {
            let manifest_path = root.join(".claude-plugin").join("plugin.json");
            let text = match fs::read_to_string(&manifest_path) {
                Ok(t) => {
                    counters.file_read();
                    debug!(plugin = %plugin, path = %manifest_path.display(), bytes = t.len(), "read_to_string: plugin manifest");
                    t
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Box::new(iter::empty()),
                Err(e) => return Box::new(iter::once(Err(e))),
            };
            let manifest: PluginManifest = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => return Box::new(iter::once(Err(io::Error::other(e)))),
            };
            Box::new(
                manifest
                    .mcp_servers
                    .into_keys()
                    .map(move |name| {
                        Ok(ConfigFile::PluginMcp {
                            plugin: plugin.clone(),
                            name,
                            path: manifest_path.clone(),
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        })
        .flatten(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::home::ClaudeHome;
    use crate::project::Project;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Build a fake claude-home dir populated with user-scope config.
    fn fixture_user_home() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().to_path_buf();
        write(&claude.join("settings.json"), "{}");
        write(&claude.join("CLAUDE.md"), "user memory");
        write(&claude.join("skills/python/SKILL.md"), "---\n---\n");
        write(&claude.join("skills/rust/SKILL.md"), "---\n---\n");
        write(&claude.join("skills/rust/rules/clippy.md"), "rule");
        write(&claude.join("commands/summary.md"), "cmd");
        write(&claude.join("agents/explorer.md"), "agent");

        let install = claude.join("plugins/cache/gage/gage/0.1.0");
        write(
            &install.join(".claude-plugin/plugin.json"),
            r#"{"name":"gage","version":"0.1.0","mcpServers":{"gage":{"command":"gage"}}}"#,
        );
        write(
            &install.join("skills/session-review/SKILL.md"),
            "---\n---\n",
        );
        write(&install.join("hooks/on-stop.json"), "{}");

        let installed_path = install.to_string_lossy().replace('\\', "/");
        let installed = format!(
            r#"{{"version":2,"plugins":{{"gage@gage":[{{"installPath":"{}"}}]}}}}"#,
            installed_path
        );
        write(&claude.join("plugins/installed_plugins.json"), &installed);

        tmp
    }

    #[test]
    fn projects_lists_extant_paths_from_claude_json() {
        use crate::session::encode_project_dir;

        let tmp = tempfile::tempdir().unwrap();
        let alice = tmp.path().join("alice");
        let bob = tmp.path().join("bob");
        let no_sessions = tmp.path().join("no_sessions");
        fs::create_dir_all(&alice).unwrap();
        fs::create_dir_all(&bob).unwrap();
        fs::create_dir_all(&no_sessions).unwrap();
        // Sessions dirs back alice and bob but not no_sessions.
        let sessions_root = tmp.path().join("projects");
        fs::create_dir_all(sessions_root.join(encode_project_dir(&alice))).unwrap();
        fs::create_dir_all(sessions_root.join(encode_project_dir(&bob))).unwrap();

        let claude_json = tmp.path().join(".claude.json");
        let body = format!(
            r#"{{"projects":{{"{}":{{}},"{}":{{}},"{}":{{}},"/nonexistent/stale":{{}}}}}}"#,
            alice.to_string_lossy(),
            bob.to_string_lossy(),
            no_sessions.to_string_lossy(),
        );
        fs::write(&claude_json, body).unwrap();
        let home = ClaudeHome::new(tmp.path().to_path_buf());
        let mut paths: Vec<PathBuf> = home
            .projects()
            .unwrap()
            .into_iter()
            .map(|p| p.path)
            .collect();
        paths.sort();
        // `/nonexistent/stale` (no on-disk dir) and `no_sessions` (no
        // sessions dir backing it) are both dropped.
        assert_eq!(paths, vec![alice, bob]);
    }

    #[test]
    fn projects_missing_claude_json_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let home = ClaudeHome::new(tmp.path().to_path_buf());
        let err = home.projects().err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn user_scope_emits_expected_variants() {
        let tmp = fixture_user_home();
        let home = ClaudeHome::new(tmp.path().to_path_buf());
        let rows: Vec<ConfigFile> = home.config().find().map(|r| r.unwrap()).collect();

        let mut have_settings = false;
        let mut have_root_memory = false;
        let mut skills: Vec<String> = Vec::new();
        let mut skill_rules: Vec<(String, String)> = Vec::new();
        let mut commands: Vec<String> = Vec::new();
        let mut agents: Vec<String> = Vec::new();
        let mut have_installed = false;
        let mut plugin_skills: Vec<(String, String)> = Vec::new();
        let mut plugin_hooks: Vec<(String, String)> = Vec::new();
        let mut plugin_mcps: Vec<(String, String)> = Vec::new();
        for r in rows {
            match r {
                ConfigFile::Settings(_) => have_settings = true,
                ConfigFile::Memory(_) => have_root_memory = true,
                ConfigFile::Skill { name, .. } => skills.push(name),
                ConfigFile::SkillRule { skill, name, .. } => skill_rules.push((skill, name)),
                ConfigFile::Command { name, .. } => commands.push(name),
                ConfigFile::Agent { name, .. } => agents.push(name),
                ConfigFile::InstalledPlugins(_) => have_installed = true,
                ConfigFile::PluginSkill { plugin, name, .. } => plugin_skills.push((plugin, name)),
                ConfigFile::PluginHook { plugin, name, .. } => plugin_hooks.push((plugin, name)),
                ConfigFile::PluginMcp { plugin, name, .. } => plugin_mcps.push((plugin, name)),
                other => panic!("unexpected variant: {other:?}"),
            }
        }

        assert!(have_settings);
        assert!(have_root_memory);
        skills.sort();
        assert_eq!(skills, vec!["python", "rust"]);
        assert_eq!(skill_rules, vec![("rust".into(), "clippy".into())]);
        assert_eq!(commands, vec!["summary"]);
        assert_eq!(agents, vec!["explorer"]);
        assert!(have_installed);
        assert_eq!(
            plugin_skills,
            vec![("gage@gage".into(), "session-review".into())]
        );
        assert_eq!(plugin_hooks, vec![("gage@gage".into(), "on-stop".into())]);
        assert_eq!(plugin_mcps, vec![("gage@gage".into(), "gage".into())]);
    }

    #[test]
    fn project_scope_emits_root_memory_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("proj");
        fs::create_dir_all(&project_root).unwrap();

        write(&project_root.join("CLAUDE.md"), "root");
        write(&project_root.join("CLAUDE.local.md"), "root-local");
        // A nested CLAUDE.md must NOT be picked up.
        write(&project_root.join("docs/CLAUDE.md"), "docs");
        write(&project_root.join(".claude/settings.json"), "{}");
        write(&project_root.join(".claude/settings.local.json"), "{}");

        let project = Project {
            path: project_root.clone(),
        };
        let rows: Vec<ConfigFile> = project.config().find().map(|r| r.unwrap()).collect();

        let has_memory = rows.iter().any(|r| matches!(r, ConfigFile::Memory(_)));
        let has_local_memory = rows.iter().any(|r| matches!(r, ConfigFile::LocalMemory(_)));
        let nested = rows.iter().any(|r| match r {
            ConfigFile::Memory(p) | ConfigFile::LocalMemory(p) => p.parent() != Some(&project_root),
            _ => false,
        });
        assert!(has_memory);
        assert!(has_local_memory);
        assert!(!nested, "nested CLAUDE.md must not be discovered");

        let has_settings = rows.iter().any(|r| matches!(r, ConfigFile::Settings(_)));
        let has_local = rows
            .iter()
            .any(|r| matches!(r, ConfigFile::LocalSettings(_)));
        assert!(has_settings);
        assert!(has_local);
    }

    #[test]
    fn project_with_missing_root_yields_no_rows() {
        // With no project tree on disk, every phase's `check_file`
        // reports NotFound (silently) and the walk produces nothing.
        let project = Project {
            path: PathBuf::from("/nonexistent/path/that/does/not/exist"),
        };
        let rows: Vec<io::Result<ConfigFile>> = project.config().find().collect();
        assert!(rows.is_empty());
    }

    #[test]
    fn config_file_io_accessors_work() {
        let tmp = fixture_user_home();
        let home = ClaudeHome::new(tmp.path().to_path_buf());
        let row = home
            .config()
            .find()
            .map(|r| r.unwrap())
            .find(|c| matches!(c, ConfigFile::Settings(_)))
            .unwrap();
        let text = row.read_to_string().unwrap();
        assert_eq!(text, "{}");
        assert_eq!(row.len().unwrap(), 2);
        assert!(row.modified().unwrap() > 0);
    }
}
