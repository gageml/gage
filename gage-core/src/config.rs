use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::glob::glob_match;

/// Returns the gage home directory.
///
/// If `GAGE_HOME` is set, uses that value. Otherwise uses `$HOME/.gage`.
pub fn gage_home() -> PathBuf {
    if let Ok(home) = env::var("GAGE_HOME") {
        PathBuf::from(home)
    } else {
        let home = env::var("HOME").expect("HOME environment variable not set");
        PathBuf::from(home).join(".gage")
    }
}

/// Returns the user config file path: `<gage_home>/config.toml`.
pub fn user_config_path() -> PathBuf {
    gage_home().join("config.toml")
}

/// Returns the plugin marketplace directory: `~/.gage/.plugin-marketplace`.
pub fn plugin_marketplace_dir() -> PathBuf {
    gage_home().join(".plugin-marketplace")
}

/// Display-friendly path for the user config file.
///
/// Uses `~` in place of `$HOME` when `GAGE_HOME` is not set.
pub fn display_user_config_path() -> String {
    if env::var("GAGE_HOME").is_ok() {
        user_config_path().to_string_lossy().into_owned()
    } else {
        "~/.gage/config.toml".to_string()
    }
}

/// Contents of a single `.gage/config.toml`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub scanners: ScannerConfig,

    /// Backup destinations. Read from the user config only; `gage push`
    /// writes to every configured remote.
    #[serde(default, rename = "remote", skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<Remote>,

    #[serde(default)]
    pub query: QueryConfig,
}

/// Query-tool settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Queries whose wall-clock time meets or exceeds this many
    /// milliseconds are written to `<gage_home>/log/slow.jsonl`. Failed
    /// queries are logged regardless of duration. A value of `0` disables
    /// the slow query log.
    #[serde(default = "default_slow_log_ms")]
    pub slow_log_ms: u64,
}

fn default_slow_log_ms() -> u64 {
    1000
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            slow_log_ms: default_slow_log_ms(),
        }
    }
}

/// A single backup destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    #[serde(flatten)]
    pub kind: RemoteKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RemoteKind {
    /// SSH remote. `url` is an rsync-style target, e.g.
    /// `user@host:/path/to/dir` or `host:/path/to/dir`.
    Ssh { url: String },
    /// S3 (or S3-compatible) remote. `url` is `s3://bucket[/prefix]`.
    /// Credentials and region come from the standard AWS chain.
    S3 {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
    },
    /// Local directory remote. Files are copied directly. `path` may use
    /// a leading `~` for `$HOME` and may be relative to the current
    /// working directory.
    Local { path: PathBuf },
}

/// Resolves a local-remote path: expands a leading `~`/`~/` to `$HOME`,
/// then joins relative paths against the current working directory.
pub fn resolve_local_path(p: &Path) -> io::Result<PathBuf> {
    let expanded = expand_tilde(p);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if s == "~" {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScannerConfig {
    /// When non-empty, only these scanners are enabled. Otherwise all
    /// scanners not in `disable` are enabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,

    /// Scanners that are explicitly disabled. Takes precedence over
    /// `enable`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

impl Config {
    /// Load the user config from the default path. Returns defaults if
    /// the file does not exist.
    pub fn load_user() -> io::Result<Self> {
        Self::load_from(&user_config_path())
    }

    pub fn load_from(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, s)
    }

    /// Merge `other` into `self`. On scalar collisions, `self` wins. Lists
    /// from `other` are appended onto `self`'s lists.
    ///
    /// Used to fold an outer (farther-from-cwd) config into an inner one,
    /// so the inner overrides scalars and lists accumulate from outer to
    /// inner.
    pub fn merge_outer(&mut self, other: Config) {
        self.scanners.enable.extend(other.scanners.enable);
        self.scanners.disable.extend(other.scanners.disable);
        self.remotes.extend(other.remotes);
    }

    /// True if the named scanner is enabled per this config. Entries
    /// in `enable` / `disable` may contain `*` wildcards that match any
    /// run of characters.
    pub fn is_scanner_enabled(&self, name: &str) -> bool {
        if self.scanners.disable.iter().any(|p| glob_match(p, name)) {
            return false;
        }
        if self.scanners.enable.is_empty() {
            return true;
        }
        self.scanners.enable.iter().any(|p| glob_match(p, name))
    }
}

/// A loaded config file along with the path it came from.
#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub path: PathBuf,
    pub config: Config,
}

/// Discovers `.gage/config.toml` files starting at `start` and walking up
/// to filesystem root, then the user config at `~/.gage/config.toml`.
///
/// Results are ordered inner-most first (closest to `start`) to outer-most
/// last (user config). Stops walking when it reaches the user's gage home
/// (so the user config is included exactly once at the end).
///
/// Only paths that actually exist are returned.
pub fn discover_config_paths(start: &Path) -> Vec<PathBuf> {
    let home = env::var("HOME").ok().map(PathBuf::from);
    let user_cfg = user_config_path();

    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if let Some(h) = &home
            && dir == h.as_path()
        {
            break;
        }
        let candidate = dir.join(".gage").join("config.toml");
        if candidate.is_file() {
            out.push(candidate);
        }
        cur = dir.parent();
    }
    if user_cfg.is_file() {
        out.push(user_cfg);
    }
    out
}

/// Load and merge configs found by walking up from `start`.
///
/// Returns the merged `Config` and the list of `ConfigSource`s in
/// inner-to-outer order (suitable for "Source" reporting).
pub fn load_merged(start: &Path) -> io::Result<(Config, Vec<ConfigSource>)> {
    let paths = discover_config_paths(start);
    let mut sources = Vec::with_capacity(paths.len());
    for p in paths {
        let cfg = Config::load_from(&p)?;
        sources.push(ConfigSource {
            path: p,
            config: cfg,
        });
    }
    let mut merged = Config::default();
    let mut iter = sources.iter();
    if let Some(first) = iter.next() {
        merged = first.config.clone();
        for s in iter {
            merged.merge_outer(s.config.clone());
        }
    }
    Ok((merged, sources))
}

/// Sentinels used to identify a "project" root when no `.gage` dir exists.
pub const PROJECT_SENTINELS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    ".vscode",
];

/// Finds the nearest project root above (and including) `start` by
/// looking for common sentinels. Stops at `$HOME` (won't return $HOME or
/// any ancestor of it).
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let home = env::var("HOME").ok().map(PathBuf::from);
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if let Some(h) = &home
            && dir == h.as_path()
        {
            return None;
        }
        for s in PROJECT_SENTINELS {
            if dir.join(s).exists() {
                return Some(dir.to_path_buf());
            }
        }
        cur = dir.parent();
    }
    None
}

/// Finds the nearest project `.gage` dir above (and including) `start`.
/// Stops at `$HOME` (exclusive) so that `~/.gage` is never returned as
/// a project config. Returns the parent directory (the "project"
/// directory that contains `.gage`), not `.gage` itself.
pub fn find_project_gage_dir(start: &Path) -> Option<PathBuf> {
    let home = env::var("HOME").ok().map(PathBuf::from);
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if let Some(h) = &home
            && dir == h.as_path()
        {
            return None;
        }
        let g = dir.join(".gage");
        if g.is_dir() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn scanner_enable_disable_globs() {
        let mut config = Config::default();
        config.scanners.disable.push("hidden-*".to_string());
        assert!(!config.is_scanner_enabled("hidden-thinking"));
        assert!(config.is_scanner_enabled("general-issues"));
    }
}
