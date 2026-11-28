use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize, de};
use serde_json as json;
use toml_edit::{DocumentMut, Item};

use crate::glob::glob_match;

/// A byte count parsed from and rendered as a human string. Accepts a
/// bare integer (bytes) or a `<number><unit>` form with a `B`, `KB`,
/// `MB`, or `GB` suffix, base 1024 and case-insensitive. Serializes to
/// the largest unit that divides it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

impl ByteSize {
    pub fn bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let upper = s.to_ascii_uppercase();
        let (num, unit) = match upper.strip_suffix("GB") {
            Some(n) => (n, GB),
            None => match upper.strip_suffix("MB") {
                Some(n) => (n, MB),
                None => match upper.strip_suffix("KB") {
                    Some(n) => (n, KB),
                    None => match upper.strip_suffix('B') {
                        Some(n) => (n, 1),
                        None => (upper.as_str(), 1),
                    },
                },
            },
        };
        let value: u64 = num
            .trim()
            .parse()
            .map_err(|e| format!("invalid size {s:?}: {e}"))?;
        value
            .checked_mul(unit)
            .map(ByteSize)
            .ok_or_else(|| format!("size overflows: {s}"))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        if n != 0 && n.is_multiple_of(GB) {
            write!(f, "{}GB", n / GB)
        } else if n != 0 && n.is_multiple_of(MB) {
            write!(f, "{}MB", n / MB)
        } else if n != 0 && n.is_multiple_of(KB) {
            write!(f, "{}KB", n / KB)
        } else {
            write!(f, "{n}B")
        }
    }
}

impl Serialize for ByteSize {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = ByteSize;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a byte count as an integer or a string like \"256MB\"")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_e| E::custom("size must not be negative"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                v.parse().map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

/// Returns the gage home directory.
///
/// If `GAGE_HOME` is set, uses that value. Otherwise uses `$HOME/.gage`.
pub fn gage_home() -> PathBuf {
    if let Ok(home) = env::var("GAGE_HOME") {
        PathBuf::from(home)
    } else {
        let home = env::var("HOME").expect("HOME should be set");
        PathBuf::from(home).join(".gage")
    }
}

/// The base directory for derived caches when a caller names none:
/// `<gage_home>/cache`. A writer of derived data takes its cache root
/// from the caller, through the options or builder that opens it,
/// and falls back to this value.
pub fn default_cache_root() -> PathBuf {
    gage_home().join("cache")
}

/// Returns the agent session corpus root: `<gage_home>/claude`. Agent
/// session JSONLs are archived here per agent name, in the same layout
/// as the Claude projects dir.
pub fn agent_sessions_dir() -> PathBuf {
    gage_home().join("claude")
}

/// Returns the user config file path: `<gage_home>/config.toml`.
pub fn user_config_path() -> PathBuf {
    gage_home().join("config.toml")
}

/// Returns the plugin marketplace directory: `~/.gage/.plugin-marketplace`.
pub fn plugin_marketplace_dir() -> PathBuf {
    gage_home().join(".plugin-marketplace")
}

/// Display-friendly path for the user config file, with `~` in place
/// of `$HOME`.
pub fn display_user_config_path() -> String {
    crate::path::shorten_home(&user_config_path())
}

/// Contents of a single `.gage/config.toml`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub scanners: ScannerConfig,

    #[serde(default)]
    pub filters: FilterConfig,

    #[serde(default)]
    pub issues: IssuesConfig,

    #[serde(default)]
    pub storage: StorageConfig,

    #[serde(default)]
    pub backup: BackupConfig,

    #[serde(default)]
    pub drivers: DriversConfig,

    /// Names for native sources, one `name = "source"` entry each
    /// under `[native]`. A name is accepted wherever a source is and
    /// is read as its source, which is `SCHEME:PATH` or a scheme
    /// alone. A name is looked up before a scheme. The name
    /// [`DEFAULT_NATIVE`] is the source a command reads when it is
    /// given none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native: BTreeMap<String, String>,
}

/// The key under `[native]` that holds the default native source:
/// the source a command reads when it is given none. Every other key
/// in the table is a name a user chose.
pub const DEFAULT_NATIVE: &str = "default";

/// Contents of a project config file, `<project>/.gage/config.toml`.
/// A project sets native sources and its store and nothing else, so a
/// file with any other table does not load.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// The `[native]` table, as [`Config::native`] holds it
    #[serde(default)]
    pub native: BTreeMap<String, String>,

    #[serde(default)]
    pub store: ProjectStoreConfig,
}

/// The `[store]` table of a project config.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectStoreConfig {
    /// The project's store, as written: a path relative to the
    /// project directory that names a place under its `.gage`
    /// directory. [`project_store`] reads it as a full path.
    pub path: Option<PathBuf>,
}

impl ProjectConfig {
    pub fn load_from(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        toml::from_str(&text).map_err(io::Error::other)
    }
}

/// The store a project config names
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStore {
    /// The config file that names the store
    pub config: PathBuf,
    /// The store, under the `.gage` directory that holds `config`
    pub path: PathBuf,
}

/// The store of the nearest project that names one, walking up from
/// `start`. A project config that does not load is an error, and so
/// is a `store.path` that names a place outside the project's `.gage`
/// directory: a project config arrives with a cloned repository, and
/// it must not point a command at a store elsewhere on the machine.
/// A config above the one that names the store is not read.
pub fn project_store(start: &Path) -> io::Result<Option<ProjectStore>> {
    for config in project_config_paths(start) {
        let at = |e: &dyn fmt::Display| io::Error::other(format!("{}: {e}", config.display()));
        let Some(written) = ProjectConfig::load_from(&config)
            .map_err(|e| at(&e))?
            .store
            .path
        else {
            continue;
        };
        let project_dir = config
            .parent()
            .and_then(Path::parent)
            .expect("a project config should be at <project>/.gage/config.toml");
        let path = project_store_path(project_dir, &written).map_err(|e| at(&e))?;
        return Ok(Some(ProjectStore { config, path }));
    }
    Ok(None)
}

/// `written` placed under `project_dir`, when it names a place under
/// the project's `.gage` directory. The check is on the path as
/// written, so `..` is refused wherever it appears.
fn project_store_path(project_dir: &Path, written: &Path) -> Result<PathBuf, String> {
    let parts: Vec<Component<'_>> = written
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect();
    let under_gage_dir = parts.len() > 1
        && parts.first() == Some(&Component::Normal(OsStr::new(".gage")))
        && parts
            .iter()
            .all(|part| matches!(part, Component::Normal(_)));
    if !under_gage_dir {
        return Err(format!(
            "store.path: {} is not a path under the project's .gage directory, such as \
             .gage/store.git",
            written.display()
        ));
    }
    Ok(project_dir.join(parts.iter().collect::<PathBuf>()))
}

/// Set `name` to `source` in the `[native]` table of the config file
/// at `path`, which is a user config or a project config. The rest of
/// the file's text stays as written, and a comment on the line of an
/// entry whose source is replaced stays on it. A file with no
/// `[native]` table gains one at its end. A missing file is created
/// with that table alone.
pub fn set_native_source(path: &Path, name: &str, source: &str) -> io::Result<()> {
    edit_document(path, |doc| {
        let table = doc
            .entry("native")
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()
            .ok_or_else(|| not_a_table(path))?;
        match table.get_mut(name).and_then(Item::as_value_mut) {
            Some(slot) => replace_value(slot, source),
            None => {
                table.insert(name, toml_edit::value(source));
            }
        }
        Ok(true)
    })
    .map(|_changed| ())
}

/// Remove `name` from the `[native]` table of the config file at
/// `path`, with the comment lines above it. The rest of the file's
/// text stays as written. Returns whether the file held the name.
pub fn remove_native_source(path: &Path, name: &str) -> io::Result<bool> {
    edit_document(path, |doc| {
        let Some(native) = doc.get_mut("native") else {
            return Ok(false);
        };
        let table = native
            .as_table_like_mut()
            .ok_or_else(|| not_a_table(path))?;
        Ok(table.remove(name).is_some())
    })
}

/// Give the entry `name` in the `[native]` table of the config file
/// at `path` the name `new_name`. The entry keeps its source and its
/// comments and becomes the last entry of the table. The rest of the
/// file's text stays as written. Returns whether the file held the
/// name.
pub fn rename_native_source(path: &Path, name: &str, new_name: &str) -> io::Result<bool> {
    edit_document(path, |doc| {
        let Some(native) = doc.get_mut("native") else {
            return Ok(false);
        };
        let table = native
            .as_table_like_mut()
            .ok_or_else(|| not_a_table(path))?;
        let Some(decor) = table.key(name).map(|key| key.leaf_decor().clone()) else {
            return Ok(false);
        };
        let item = table.remove(name).expect("a key should have an item");
        table.insert(new_name, item);
        let mut key = table
            .key_mut(new_name)
            .expect("an inserted item should have a key");
        *key.leaf_decor_mut() = decor;
        Ok(true)
    })
}

/// Replace each `name` in the `backup.native` list of the config file
/// at `path` with `new_name`. The rest of the file's text stays as
/// written. Returns whether the list held the name.
pub fn rename_backup_native(path: &Path, name: &str, new_name: &str) -> io::Result<bool> {
    edit_document(path, |doc| {
        let Some(list) = doc
            .get_mut("backup")
            .and_then(|backup| backup.get_mut("native"))
            .and_then(Item::as_array_mut)
        else {
            return Ok(false);
        };
        let mut changed = false;
        for slot in list.iter_mut().filter(|slot| slot.as_str() == Some(name)) {
            replace_value(slot, new_name);
            changed = true;
        }
        Ok(changed)
    })
}

/// Apply `edit` to the TOML document at `path` and write the document
/// back when `edit` returns true. A missing file is read as an empty
/// document. Returns what `edit` returned.
fn edit_document(
    path: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> io::Result<bool>,
) -> io::Result<bool> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc: DocumentMut = text.parse().map_err(io::Error::other)?;
    if !edit(&mut doc)? {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, doc.to_string())?;
    Ok(true)
}

/// Put the string `value` in `slot`. A comment on the line is the old
/// value's decoration and moves to the new one.
fn replace_value(slot: &mut toml_edit::Value, value: &str) {
    let decor = slot.decor().clone();
    *slot = toml_edit::Value::from(value);
    *slot.decor_mut() = decor;
}

fn not_a_table(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: native is not a table", path.display()),
    )
}

/// Each `<dir>/.gage` directory that exists walking up from `start`,
/// innermost first. The walk stops at `$HOME`, so it never reaches
/// the user's gage home there.
pub fn project_gage_dirs(start: &Path) -> Vec<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if home.as_deref() == Some(dir) {
            break;
        }
        let candidate = dir.join(".gage");
        if candidate.is_dir() {
            out.push(candidate);
        }
        cur = dir.parent();
    }
    out
}

/// Each project config file, `<dir>/.gage/config.toml`, that exists
/// walking up from `start`, innermost first. The gage home is the
/// user's and is never a project, wherever `GAGE_HOME` puts it.
pub fn project_config_paths(start: &Path) -> Vec<PathBuf> {
    let user = gage_home();
    project_gage_dirs(start)
        .into_iter()
        .filter(|dir| *dir != user)
        .map(|dir| dir.join("config.toml"))
        .filter(|path| path.is_file())
        .collect()
}

/// The driver a build treats as default when the config names none
pub const DEFAULT_DRIVER: &str = "claude";

/// Session driver settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriversConfig {
    /// The driver an agent model with no scheme goes to, by name,
    /// and the driver whose default location the empty source is
    /// when the config sets no default native source
    #[serde(default = "default_driver")]
    pub default: String,

    /// Per-driver settings, one `[drivers.<name>]` table each. The
    /// name is the driver's; `default` is the setting above, so no
    /// driver has that name.
    #[serde(default, flatten)]
    pub entries: BTreeMap<String, DriverEntry>,
}

/// Settings for one driver.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverEntry {
    /// The model each size name resolves to on this driver, set under
    /// `[drivers.<name>.models]`. A size set here replaces the
    /// driver's default model for it.
    #[serde(default, skip_serializing_if = "ModelsConfig::is_empty")]
    pub models: ModelsConfig,
}

/// A model for each size name a driver resolves.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large: Option<String>,
}

impl ModelsConfig {
    pub fn is_empty(&self) -> bool {
        self.small.is_none() && self.medium.is_none() && self.large.is_none()
    }
}

fn default_driver() -> String {
    DEFAULT_DRIVER.to_string()
}

impl Default for DriversConfig {
    fn default() -> Self {
        Self {
            default: default_driver(),
            entries: BTreeMap::new(),
        }
    }
}

/// `gage backup` settings.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    /// Sources whose sessions are added to the store before each
    /// backup. `gage backup --native` replaces the list for one run;
    /// `--stored` skips it.
    #[serde(default)]
    pub native: Vec<String>,
}

/// Session storage settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// The largest total content a single session may write to the
    /// store. `gage session add` refuses a session whose stored files
    /// exceed this; `--max-size` overrides it for one run.
    #[serde(default = "default_max_session_size")]
    pub max_session_size: ByteSize,
}

fn default_max_session_size() -> ByteSize {
    ByteSize(256 * MB)
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_session_size: default_max_session_size(),
        }
    }
}

/// Issue settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuesConfig {
    /// Similarity score at or above which two issues are reported as
    /// related (see `related_issue` and `gage issue show`).
    #[serde(default = "default_related_threshold")]
    pub related_threshold: f64,
}

fn default_related_threshold() -> f64 {
    0.2
}

impl Default for IssuesConfig {
    fn default() -> Self {
        Self {
            related_threshold: default_related_threshold(),
        }
    }
}

/// The Git repository `gage scanner` reads when the config names none.
pub const DEFAULT_SCANNER_SOURCE: &str = "https://github.com/gageml/gage-scanners.git";

fn default_scanner_source() -> String {
    DEFAULT_SCANNER_SOURCE.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannerConfig {
    /// When non-empty, only these scanners are enabled. Otherwise all
    /// scanners not in `disable` are enabled.
    #[serde(default)]
    pub enable: Vec<String>,

    /// Scanners that are explicitly disabled. Takes precedence over
    /// `enable`.
    #[serde(default)]
    pub disable: Vec<String>,

    /// The Git repository scanners are installed from: a URL or the
    /// path of a local repository.
    #[serde(default = "default_scanner_source")]
    pub source: String,

    /// Per-scanner settings, one `[scanners.<name>]` table each.
    #[serde(default, flatten)]
    pub entries: BTreeMap<String, ScannerEntry>,
}

/// Settings for one scanner.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScannerEntry {
    /// Values for the scanner's declared params, set under
    /// `[scanners.<name>.params]`. A value here replaces the default
    /// the scanner declares; a `--scanner` override replaces both.
    #[serde(default, skip_serializing_if = "json::Map::is_empty")]
    pub params: json::Map<String, json::Value>,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        ScannerConfig {
            enable: Vec::new(),
            disable: Vec::new(),
            source: default_scanner_source(),
            entries: BTreeMap::new(),
        }
    }
}

impl ScannerConfig {
    /// The params the config sets for the scanner with the full name
    /// `name`: those of the table under the full name, and otherwise
    /// those of the table under the name without its namespace.
    pub fn params(&self, name: &str) -> Option<&json::Map<String, json::Value>> {
        self.entries
            .get(name)
            .or_else(|| self.entries.get(scanner_short_name(name)))
            .map(|e| &e.params)
    }
}

/// The part of a scanner's full name after its namespace.
fn scanner_short_name(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(_, short)| short)
}

/// Whether a config pattern names the scanner with the full name
/// `name`. A pattern with a `/` is matched against the full name, and
/// a pattern without one against the name without its namespace, so
/// it names that scanner in any namespace.
fn scanner_pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern.contains('/') {
        glob_match(pattern, name)
    } else {
        glob_match(pattern, scanner_short_name(name))
    }
}

/// Publish filter settings.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Per-filter settings, one `[filters.<name>]` table each.
    #[serde(default, flatten)]
    pub entries: BTreeMap<String, FilterEntry>,
}

/// Settings for one publish filter.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FilterEntry {
    /// Values for the filter's declared params, set under
    /// `[filters.<name>.params]`. A value here replaces the default
    /// the filter declares; a `--filter` override replaces both.
    #[serde(default, skip_serializing_if = "json::Map::is_empty")]
    pub params: json::Map<String, json::Value>,
}

impl FilterConfig {
    /// The params the config sets for the named filter.
    pub fn params(&self, name: &str) -> Option<&json::Map<String, json::Value>> {
        self.entries.get(name).map(|e| &e.params)
    }
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

    /// Name `driver` as the default driver in the config file at
    /// `path`, when the file names none. A missing file is created
    /// with every setting at its default. A file that sets other
    /// things gains a `[drivers]` table at its end, so its comments
    /// and layout stay as written. A file that already opens a
    /// `[drivers]` table without a default cannot take a second one
    /// and is an error. Returns whether the file changed.
    pub fn name_default_driver(path: &Path, driver: &str) -> io::Result<bool> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut config = Config::default();
                config.drivers.default = driver.to_string();
                config.save_to(path)?;
                return Ok(true);
            }
            Err(e) => return Err(e),
        };
        let table: toml::Table = toml::from_str(&text).map_err(io::Error::other)?;
        if let Some(toml::Value::Table(drivers)) = table.get("drivers")
            && drivers.contains_key("default")
        {
            return Ok(false);
        }
        let separator = if text.is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let table = toml::to_string(&toml::Table::from_iter([(
            "drivers".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "default".to_string(),
                toml::Value::String(driver.to_string()),
            )])),
        )]))
        .map_err(io::Error::other)?;
        let text = format!("{text}{separator}{table}");
        // A file with only `[drivers.<name>]` tables takes the
        // `[drivers]` table after them; one that opens `[drivers]`
        // itself does not parse with a second.
        if toml::from_str::<toml::Table>(&text).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} has a drivers table with no default; add default = {driver:?} to it",
                    path.display()
                ),
            ));
        }
        fs::write(path, text)?;
        Ok(true)
    }

    /// The default driver the config file at `path` names itself. A
    /// missing file, and a file that leaves the setting to its
    /// default, name none.
    pub fn named_default_driver(path: &Path) -> io::Result<Option<String>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let table: toml::Table = toml::from_str(&text).map_err(io::Error::other)?;
        let named = table
            .get("drivers")
            .and_then(|drivers| drivers.get("default"))
            .and_then(toml::Value::as_str);
        Ok(named.map(String::from))
    }

    /// Make `driver` the default driver in the config file at `path`,
    /// whether or not the file names one. A file that names another
    /// has that one value replaced, and the rest of its text stays as
    /// written. Returns whether the file changed.
    pub fn set_default_driver(path: &Path, driver: &str) -> io::Result<bool> {
        match Self::named_default_driver(path)? {
            None => Self::name_default_driver(path, driver),
            Some(named) if named == driver => Ok(false),
            Some(_) => {
                let text = fs::read_to_string(path)?;
                let mut doc: DocumentMut = text.parse().map_err(io::Error::other)?;
                let slot = doc
                    .get_mut("drivers")
                    .and_then(|drivers| drivers.get_mut("default"))
                    .and_then(Item::as_value_mut)
                    .expect("a named default should be a value");
                replace_value(slot, driver);
                fs::write(path, doc.to_string())?;
                Ok(true)
            }
        }
    }

    /// True if the scanner with the full name `name` is enabled per
    /// this config. Entries in `enable` / `disable` may contain `*`
    /// wildcards that match any run of characters. An entry without a
    /// `/` names a scanner in any namespace.
    pub fn is_scanner_enabled(&self, name: &str) -> bool {
        if self.is_scanner_disabled(name) {
            return false;
        }
        if self.scanners.enable.is_empty() {
            return true;
        }
        self.scanners
            .enable
            .iter()
            .any(|p| scanner_pattern_matches(p, name))
    }

    /// True if the named scanner is explicitly disabled. Unlike
    /// `is_scanner_enabled`, absence from a non-empty `enable` list
    /// does not count — this consults the `disable` list only.
    pub fn is_scanner_disabled(&self, name: &str) -> bool {
        self.scanners
            .disable
            .iter()
            .any(|p| scanner_pattern_matches(p, name))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        Config, DEFAULT_NATIVE, ProjectConfig, project_config_paths, project_store,
        remove_native_source, rename_backup_native, rename_native_source, set_native_source,
    };
    use serde_json as json;

    #[test]
    fn default_driver_is_claude_until_the_config_names_one() {
        assert_eq!(Config::default().drivers.default, "claude");
        let config: Config = toml::from_str("[drivers]\ndefault = \"codex\"\n").unwrap();
        assert_eq!(config.drivers.default, "codex");
    }

    #[test]
    fn native_names_are_read_from_their_table_and_left_out_when_none_are_set() {
        assert!(Config::default().native.is_empty());
        let unset = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!unset.contains("[native]"), "{unset}");
        let config: Config = toml::from_str("[native]\nwork = \"pi:~/x\"\n").unwrap();
        assert_eq!(config.native["work"], "pi:~/x");
        let set = toml::to_string_pretty(&config).unwrap();
        assert!(set.contains("[native]\nwork = \"pi:~/x\"\n"), "{set}");
    }

    #[test]
    fn a_project_config_holds_native_sources_and_a_store_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[native]\ndefault = \"pi:sessions\"\n").unwrap();
        let config = ProjectConfig::load_from(&path).unwrap();
        assert_eq!(config.native[DEFAULT_NATIVE], "pi:sessions");

        fs::write(&path, "").unwrap();
        let empty = ProjectConfig::load_from(&path).unwrap();
        assert!(empty.native.is_empty());
        assert_eq!(empty.store.path, None);

        fs::write(&path, "[store]\npath = \".gage/store.git\"\n").unwrap();
        let config = ProjectConfig::load_from(&path).unwrap();
        assert_eq!(config.store.path, Some(PathBuf::from(".gage/store.git")));

        fs::write(&path, "[store]\nurl = \"x\"\n").unwrap();
        let err = ProjectConfig::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("unknown field `url`"), "{err}");

        fs::write(&path, "[drivers]\ndefault = \"pi\"\n").unwrap();
        let err = ProjectConfig::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("unknown field `drivers`"), "{err}");
    }

    #[test]
    fn setting_a_native_source_adds_a_table_or_an_entry_and_keeps_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        set_native_source(&path, "work", "pi:~/w").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[native]\nwork = \"pi:~/w\"\n"
        );

        let written = "# kept\n[backup]\nnative = [\"work\"]\n";
        fs::write(&path, written).unwrap();
        set_native_source(&path, "work", "pi:~/w").unwrap();
        set_native_source(&path, "home", "claude").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{written}\n[native]\nwork = \"pi:~/w\"\nhome = \"claude\"\n")
        );
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.native["home"], "claude");
        assert_eq!(config.backup.native, ["work"]);
    }

    #[test]
    fn setting_a_native_source_that_is_set_replaces_its_value_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "[native]\n# mine\nwork = \"pi:~/w\" # here\nold = \"codex\"\n";
        fs::write(&path, written).unwrap();
        set_native_source(&path, "work", "claude:/x").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            written.replace("pi:~/w", "claude:/x")
        );
    }

    #[test]
    fn removing_a_native_source_takes_its_line_and_says_whether_it_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(!remove_native_source(&path, "work").unwrap());
        assert!(!path.exists());

        let written =
            "[native]\nwork = \"pi:~/w\"\nold = \"codex\" # kept\n\n[backup]\nnative = []\n";
        fs::write(&path, written).unwrap();
        assert!(!remove_native_source(&path, "none").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), written);
        assert!(remove_native_source(&path, "work").unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            written.replace("work = \"pi:~/w\"\n", "")
        );
    }

    #[test]
    fn renaming_a_native_source_keeps_its_source_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "[native]\n# mine\nwork = \"pi:~/w\" # here\nold = \"codex\"\n";
        fs::write(&path, written).unwrap();
        assert!(!rename_native_source(&path, "none", "job").unwrap());
        assert!(rename_native_source(&path, "work", "job").unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[native]\nold = \"codex\"\n# mine\njob = \"pi:~/w\" # here\n"
        );
    }

    #[test]
    fn renaming_a_backup_native_source_replaces_each_entry_of_that_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "[backup]\nnative = [\"work\", \"pi:\", \"work\"] # all\n";
        fs::write(&path, written).unwrap();
        assert!(!rename_backup_native(&path, "none", "job").unwrap());
        assert!(rename_backup_native(&path, "work", "job").unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            written.replace("work", "job")
        );
    }

    #[test]
    fn project_config_paths_are_found_walking_up_innermost_first() {
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("outer");
        let inner = outer.join("mid/inner");
        fs::create_dir_all(inner.join(".gage")).unwrap();
        fs::create_dir_all(outer.join("mid/.gage")).unwrap();
        fs::create_dir_all(outer.join(".gage")).unwrap();
        fs::write(inner.join(".gage/config.toml"), "").unwrap();
        fs::write(outer.join(".gage/config.toml"), "").unwrap();
        let found: Vec<PathBuf> = project_config_paths(&inner)
            .into_iter()
            .filter(|path| path.starts_with(root.path()))
            .collect();
        assert_eq!(
            found,
            [
                inner.join(".gage/config.toml"),
                outer.join(".gage/config.toml"),
            ]
        );
    }

    #[test]
    fn a_project_store_is_the_one_the_nearest_project_names() {
        let root = tempfile::tempdir().unwrap();
        let outer = root.path().join("outer");
        let inner = outer.join("mid/inner");
        fs::create_dir_all(inner.join(".gage")).unwrap();
        fs::create_dir_all(outer.join(".gage")).unwrap();
        let outer_config = outer.join(".gage/config.toml");
        let inner_config = inner.join(".gage/config.toml");

        assert_eq!(project_store(&inner).unwrap(), None);

        fs::write(
            &outer_config,
            "[store]\npath = \"./.gage/trial/store.git\"\n",
        )
        .unwrap();
        fs::write(&inner_config, "[native]\ndefault = \"pi:sessions\"\n").unwrap();
        let store = project_store(&inner).unwrap().unwrap();
        assert_eq!(store.config, outer_config);
        assert_eq!(store.path, outer.join(".gage/trial/store.git"));

        fs::write(&inner_config, "[store]\npath = \".gage/store.git\"\n").unwrap();
        let store = project_store(&inner).unwrap().unwrap();
        assert_eq!(store.config, inner_config);
        assert_eq!(store.path, inner.join(".gage/store.git"));
        assert_eq!(
            project_store(&outer.join("mid")).unwrap().unwrap().path,
            outer.join(".gage/trial/store.git")
        );

        // The config above the one that names the store is not read
        fs::write(&outer_config, "not toml").unwrap();
        assert!(project_store(&inner).is_ok());
        let err = project_store(&outer).unwrap_err().to_string();
        assert!(
            err.starts_with(&outer_config.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_project_store_outside_the_project_gage_directory_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir_all(project.join(".gage")).unwrap();
        let config = project.join(".gage/config.toml");
        for path in [
            "store.git",
            ".gage",
            ".gage/../store.git",
            ".gage/a/../../store.git",
            "../.gage/store.git",
            "/tmp/store.git",
            "~/.gage/store.git",
            "",
        ] {
            fs::write(&config, format!("[store]\npath = {path:?}\n")).unwrap();
            let err = project_store(&project).unwrap_err().to_string();
            assert!(
                err.contains("store.path") && err.contains(".gage/config.toml"),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn naming_the_default_driver_creates_a_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::name_default_driver(&path, "codex").unwrap());
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.drivers.default, "codex");
    }

    #[test]
    fn naming_the_default_driver_appends_to_a_config_and_keeps_its_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "# kept\n[backup]\nnative = [\"claude:\"]";
        std::fs::write(&path, written).unwrap();
        assert!(Config::name_default_driver(&path, "claude").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("{written}\n\n[drivers]\ndefault = \"claude\"\n")
        );
        assert_eq!(Config::load_from(&path).unwrap().backup.native, ["claude:"]);
    }

    #[test]
    fn naming_the_default_driver_leaves_a_named_default_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "[drivers]\ndefault = \"codex\"\n";
        std::fs::write(&path, written).unwrap();
        assert!(!Config::name_default_driver(&path, "claude").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    #[test]
    fn the_named_default_driver_is_what_the_file_says_and_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::named_default_driver(&path).unwrap(), None);
        std::fs::write(&path, "[drivers.codex.models]\nsmall = \"x\"\n").unwrap();
        assert_eq!(Config::named_default_driver(&path).unwrap(), None);
        std::fs::write(&path, "[drivers]\ndefault = \"codex\"\n").unwrap();
        assert_eq!(
            Config::named_default_driver(&path).unwrap().as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn setting_the_default_driver_replaces_a_named_one_and_keeps_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "# kept\n[drivers]\ndefault = \"claude\" # mine\n\n[backup]\nnative = []\n";
        std::fs::write(&path, written).unwrap();
        assert!(Config::set_default_driver(&path, "codex").unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            written.replace("\"claude\"", "\"codex\"")
        );
        assert!(!Config::set_default_driver(&path, "codex").unwrap());
    }

    #[test]
    fn setting_the_default_driver_names_one_where_the_file_names_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::set_default_driver(&path, "codex").unwrap());
        assert_eq!(
            Config::named_default_driver(&path).unwrap().as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn naming_the_default_driver_refuses_a_drivers_table_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[drivers]\n").unwrap();
        let err = Config::name_default_driver(&path, "claude").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn naming_the_default_driver_follows_per_driver_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let written = "[drivers.claude.models]\nsmall = \"haiku\"\n";
        std::fs::write(&path, written).unwrap();
        assert!(Config::name_default_driver(&path, "claude").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("{written}\n[drivers]\ndefault = \"claude\"\n")
        );
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.drivers.default, "claude");
        assert_eq!(
            config.drivers.entries["claude"].models.small.as_deref(),
            Some("haiku")
        );
    }

    #[test]
    fn driver_models_are_read_from_a_per_driver_table_and_survive_a_save() {
        let config: Config = toml::from_str(
            r#"
[drivers]
default = "claude"

[drivers.claude.models]
medium = "claude-sonnet-5-5"
"#,
        )
        .unwrap();
        let models = &config.drivers.entries["claude"].models;
        assert_eq!(models.medium.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(models.small, None);
        let text = toml::to_string_pretty(&config).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(loaded.drivers.default, "claude");
        assert_eq!(&loaded.drivers.entries["claude"].models, models);
    }

    #[test]
    fn a_model_size_the_drivers_do_not_have_is_refused() {
        let err = toml::from_str::<Config>("[drivers.claude.models]\nhuge = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("huge"), "{err}");
    }

    #[test]
    fn scanner_enable_disable_globs() {
        let mut config = Config::default();
        config.scanners.disable.push("hidden-*".to_string());
        assert!(!config.is_scanner_enabled("gage/hidden-thinking"));
        assert!(!config.is_scanner_enabled("local/hidden-thinking"));
        assert!(config.is_scanner_enabled("gage/general"));

        // A pattern with a namespace names the scanner of that
        // namespace alone
        config.scanners.disable = vec!["gage/gen*".to_string()];
        assert!(!config.is_scanner_enabled("gage/general"));
        assert!(config.is_scanner_enabled("local/general"));
    }

    #[test]
    fn scanner_source_is_read_from_the_scanners_table() {
        assert_eq!(
            Config::default().scanners.source,
            super::DEFAULT_SCANNER_SOURCE
        );
        let config: Config = toml::from_str("[scanners]\nsource = \"/repos/scanners\"\n").unwrap();
        assert_eq!(config.scanners.source, "/repos/scanners");
        // The setting is not taken for a scanner's own table
        assert!(config.scanners.params("source").is_none());
    }

    #[test]
    fn scanner_params_are_read_from_a_per_scanner_table() {
        let config: Config = toml::from_str(
            r#"
[scanners]
disable = ["hidden-*"]

[scanners.findings.params]
wontfix = ["Actions taken without approval."]
budget = 3
"#,
        )
        .unwrap();
        assert_eq!(config.scanners.disable, ["hidden-*"]);
        assert_eq!(config.scanners.source, super::DEFAULT_SCANNER_SOURCE);
        let params = config.scanners.params("findings").unwrap();
        assert_eq!(
            params["wontfix"],
            json::json!(["Actions taken without approval."])
        );
        assert_eq!(params["budget"], json::json!(3));
        assert!(config.scanners.params("general").is_none());
    }

    #[test]
    fn filter_params_are_read_from_a_per_filter_table_and_survive_a_save() {
        let config: Config = toml::from_str(
            r#"
[filters.pii.params]
prompt = "Treat project codenames as names."
"#,
        )
        .unwrap();
        assert_eq!(
            config.filters.params("pii").unwrap()["prompt"],
            json::json!("Treat project codenames as names.")
        );
        assert!(config.filters.params("secret").is_none());
        let text = toml::to_string_pretty(&config).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(loaded.filters.params("pii"), config.filters.params("pii"));
    }

    #[test]
    fn scanner_params_survive_a_save_and_load() {
        let mut config = Config::default();
        config
            .scanners
            .entries
            .entry("findings".to_string())
            .or_default()
            .params
            .insert("wontfix".to_string(), json::json!(["a", "b"]));
        let text = toml::to_string_pretty(&config).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            loaded.scanners.params("findings").unwrap()["wontfix"],
            json::json!(["a", "b"])
        );
    }
}
