use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize, de};
use serde_json as json;

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
}

/// The driver a build treats as default when the config names none
pub const DEFAULT_DRIVER: &str = "claude";

/// Session driver settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriversConfig {
    /// The driver a source or an agent model with no scheme goes to,
    /// by name
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
    /// backup. `gage backup --source` replaces the list for one run;
    /// `--stored` skips it.
    #[serde(default)]
    pub sources: Vec<String>,
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScannerConfig {
    /// When non-empty, only these scanners are enabled. Otherwise all
    /// scanners not in `disable` are enabled.
    #[serde(default)]
    pub enable: Vec<String>,

    /// Scanners that are explicitly disabled. Takes precedence over
    /// `enable`.
    #[serde(default)]
    pub disable: Vec<String>,

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

impl ScannerConfig {
    /// The params the config sets for the named scanner.
    pub fn params(&self, name: &str) -> Option<&json::Map<String, json::Value>> {
        self.entries.get(name).map(|e| &e.params)
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
                let mut doc: toml_edit::DocumentMut = text.parse().map_err(io::Error::other)?;
                let slot = doc
                    .get_mut("drivers")
                    .and_then(|drivers| drivers.get_mut("default"))
                    .and_then(toml_edit::Item::as_value_mut)
                    .expect("a named default should be a value");
                // A comment on the setting's line is the old value's
                // decoration and moves to the new one
                let decor = slot.decor().clone();
                *slot = toml_edit::Value::from(driver);
                *slot.decor_mut() = decor;
                fs::write(path, doc.to_string())?;
                Ok(true)
            }
        }
    }

    /// True if the named scanner is enabled per this config. Entries
    /// in `enable` / `disable` may contain `*` wildcards that match any
    /// run of characters.
    pub fn is_scanner_enabled(&self, name: &str) -> bool {
        if self.is_scanner_disabled(name) {
            return false;
        }
        if self.scanners.enable.is_empty() {
            return true;
        }
        self.scanners.enable.iter().any(|p| glob_match(p, name))
    }

    /// True if the named scanner is explicitly disabled. Unlike
    /// `is_scanner_enabled`, absence from a non-empty `enable` list
    /// does not count — this consults the `disable` list only.
    pub fn is_scanner_disabled(&self, name: &str) -> bool {
        self.scanners.disable.iter().any(|p| glob_match(p, name))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use serde_json as json;

    #[test]
    fn default_driver_is_claude_until_the_config_names_one() {
        assert_eq!(Config::default().drivers.default, "claude");
        let config: Config = toml::from_str("[drivers]\ndefault = \"codex\"\n").unwrap();
        assert_eq!(config.drivers.default, "codex");
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
        let written = "# kept\n[backup]\nsources = [\"claude:\"]";
        std::fs::write(&path, written).unwrap();
        assert!(Config::name_default_driver(&path, "claude").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("{written}\n\n[drivers]\ndefault = \"claude\"\n")
        );
        assert_eq!(
            Config::load_from(&path).unwrap().backup.sources,
            ["claude:"]
        );
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
        let written = "# kept\n[drivers]\ndefault = \"claude\" # mine\n\n[backup]\nsources = []\n";
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
        assert!(!config.is_scanner_enabled("hidden-thinking"));
        assert!(config.is_scanner_enabled("general"));
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
