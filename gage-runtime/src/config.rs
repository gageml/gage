//! Rune-facing config accessors: `user().config()` for user settings,
//! `session.config()` / `project.config()` for a project's config
//! files.
//!
//! `user().config()` exposes Claude Code's user-level configuration
//! (`~/.claude/settings.json`) as a `UserConfig` value resolved
//! asynchronously, matching Claude Code's "user settings" terminology.
//!
//! `session.config()` resolves the session's project and queries the
//! `config` table for that project's config files, yielding `Config`
//! rows; `project.config()` is the same query rooted at a `Project`
//! directly. File text is never selected — `Config::read()` reads the
//! file on demand.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use gage_claude::config::ConfigFile;
use gage_claude::home::ClaudeHome;
use gage_claude::project::project_for_session_name;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Ref, Value};
use rune::{Any, ContextError, Module};
use serde_json as json;
use tracing::warn;

use crate::datetime::DateTime;
use crate::error::Error;
use crate::scan::{Session, Sessions};
use crate::state::current_scan_ctx;
use crate::value::json_to_value;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("user", user).build()?;
    m.function_meta(User::config)?;
    m.associated_function(
        &rune::runtime::Protocol::INTO_FUTURE,
        |q: UserConfigQuery| async move { fetch_user_config(q).await },
    )?;

    m.function_meta(project)?;
    m.function_meta(projects)?;
    m.function_meta(config)?;
    m.function_meta(project_config)?;
    m.associated_function("type", ConfigQuery::type_)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: ConfigQuery| async move {
        fetch_config(q).await
    })?;
    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<User>()?;
    m.ty::<UserConfigQuery>()?;
    m.ty::<UserConfig>()?;
    m.function_meta(UserConfig::get)?;

    m.ty::<Project>()?;
    m.field_function(&Protocol::GET, "path", |p: &Project| {
        p.path.to_string_lossy().into_owned()
    })?;

    m.ty::<ConfigQuery>()?;
    m.ty::<Config>()?;
    m.field_function(&Protocol::GET, "type", |c: &Config| c.type_.clone())?;
    m.field_function(&Protocol::GET, "path", |c: &Config| {
        c.path.to_string_lossy().into_owned()
    })?;
    m.function_meta(read)?;
    m.function_meta(Config::debug)?;
    Ok(())
}

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct User {}

impl User {
    #[rune::function(instance)]
    fn config(&self) -> UserConfigQuery {
        UserConfigQuery {}
    }
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct UserConfigQuery {}

fn user() -> User {
    User {}
}

async fn fetch_user_config(_q: UserConfigQuery) -> UserConfig {
    let mut config = UserConfig::empty();

    let home = match ClaudeHome::from_env() {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "config: cannot resolve Claude home");
            return config;
        }
    };

    let files = home
        .config()
        .memory(false)
        .skills(false)
        .commands(false)
        .agents(false)
        .plugins(false)
        .find();
    for f in files {
        match f {
            Ok(ConfigFile::Settings(p)) => config.settings = load_settings(&p),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "config: walk error"),
        }
    }

    config
}

/// Read `path` and parse as a JSON object. Any failure (read error,
/// parse error, root-not-object) logs at warn and yields `None`.
fn load_settings(path: &Path) -> Option<json::Map<String, json::Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "settings: read failed");
            return None;
        }
    };
    let parsed: json::Value = match json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "settings: JSON parse failed");
            return None;
        }
    };
    match parsed {
        json::Value::Object(map) => Some(map),
        other => {
            warn!(
                path = %path.display(),
                actual = ?json_kind(&other),
                "settings: root is not an object",
            );
            None
        }
    }
}

fn json_kind(v: &json::Value) -> &'static str {
    match v {
        json::Value::Null => "null",
        json::Value::Bool(_) => "bool",
        json::Value::Number(_) => "number",
        json::Value::String(_) => "string",
        json::Value::Array(_) => "array",
        json::Value::Object(_) => "object",
    }
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct UserConfig {
    #[rune(skip)]
    settings: Option<json::Map<String, json::Value>>,
}

impl UserConfig {
    fn empty() -> Self {
        Self { settings: None }
    }

    /// Resolve a dot-delimited object path against the user
    /// `settings.json`. Returns `None` if any path segment misses or
    /// hits a non-object before the final segment.
    #[rune::function(instance)]
    fn get(&self, path: String) -> Option<Value> {
        let map = self.settings.as_ref()?;
        let value = walk_object_path(map, &path)?;
        Some(json_to_value(value))
    }
}

/// Walk dotted `path` through `root`, descending only into objects.
/// Returns `None` if any segment is missing or a non-final segment
/// resolves to a non-object value.
fn walk_object_path<'a>(
    root: &'a json::Map<String, json::Value>,
    path: &str,
) -> Option<&'a json::Value> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let mut cur = root.get(first)?;
    for seg in segments {
        let json::Value::Object(map) = cur else {
            return None;
        };
        cur = map.get(seg)?;
    }
    Some(cur)
}

/// The project a session belongs to, resolved from the encoded
/// directory its JSONL lives under via the `~/.claude.json` registry.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Project {
    #[rune(skip)]
    pub path: PathBuf,
}

#[rune::function(instance)]
fn project(session: Ref<Session>) -> crate::Result<Project> {
    resolve_project(&session.src)
}

/// Distinct projects for the sessions, in iteration order. Sessions
/// whose project is not recorded are skipped — an unrecorded project
/// is a normal state (deleted project, scratchpad cwd), not a failure.
#[rune::function(instance)]
fn projects(sessions: Ref<Sessions>) -> crate::Result<Vec<Project>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for s in sessions.remaining() {
        if let Some(p) = resolve_project_opt(&s.src)?
            && seen.insert(p.path.clone())
        {
            out.push(p);
        }
    }
    Ok(out)
}

#[rune::function(instance)]
fn config(session: Ref<Session>) -> ConfigQuery {
    ConfigQuery {
        root: ConfigRoot::Session(session.src.clone()),
        type_: None,
    }
}

#[rune::function(instance, path = config)]
fn project_config(project: Ref<Project>) -> ConfigQuery {
    ConfigQuery {
        root: ConfigRoot::Project(project.path.clone()),
        type_: None,
    }
}

fn resolve_project(src: &Path) -> crate::Result<Project> {
    let name = session_dir_name(src)?;
    resolve_project_opt(src)?
        .ok_or_else(|| Error::Config(format!("no project recorded for session dir `{name}`")))
}

/// Resolve the project for a session, or `None` when the session's
/// encoded directory isn't recorded in `~/.claude.json`. An
/// unrecorded directory is a normal state (e.g. a scratchpad cwd, or
/// a project the user deleted), not a failure.
fn resolve_project_opt(src: &Path) -> crate::Result<Option<Project>> {
    let name = session_dir_name(src)?;
    let home = ClaudeHome::from_env().map_err(|e| Error::Config(e.to_string()))?;
    match project_for_session_name(&home, &name) {
        Ok(p) => Ok(p.map(|p| Project { path: p.path })),
        Err(e) => Err(Error::Config(e.to_string())),
    }
}

fn session_dir_name(src: &Path) -> crate::Result<String> {
    src.parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::Config("session path has no project directory".into()))
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct ConfigQuery {
    #[rune(skip)]
    root: ConfigRoot,
    #[rune(skip)]
    type_: Option<Value>,
}

/// Where a config query is rooted: a session (project resolved from
/// its JSONL path) or a project directly.
enum ConfigRoot {
    Session(PathBuf),
    Project(PathBuf),
}

impl ConfigQuery {
    fn type_(mut self, t: Value) -> Self {
        self.type_ = Some(t);
        self
    }
}

// `text` is deliberately not selected: the `config` table only reads
// file contents when the projection asks for them, and these rows are
// metadata. `Config::read()` fetches contents on demand.
async fn fetch_config(q: ConfigQuery) -> crate::Result<Vec<Config>> {
    let project_path = match &q.root {
        ConfigRoot::Project(p) => p.clone(),
        ConfigRoot::Session(src) => match resolve_project_opt(src)? {
            Some(p) => p.path,
            None => {
                tracing::debug!(
                    "no project recorded for session dir `{}`; no config files",
                    session_dir_name(src)?
                );
                return Ok(vec![]);
            }
        },
    };
    let ctx = current_scan_ctx();
    let df_ctx = &ctx.run.scan_ctx;

    let mut params = vec![ScalarValue::Utf8(Some(
        project_path.to_string_lossy().into_owned(),
    ))];
    let mut clauses = vec!["project = $1".to_string()];
    if let Some(t) = q.type_ {
        let spec = serde_json::to_value(&t)
            .map_err(|e| Error::Args(format!("`.type()` value could not be read: {e}")))?;
        clauses.push(type_clause(&spec, &mut params)?);
    }

    let sql = format!(
        "SELECT scope, project, \"type\", name, path, size, mtime \
         FROM config WHERE {} ORDER BY path",
        clauses.join(" AND ")
    );
    let df = df_ctx
        .sql(&sql)
        .await
        .map_err(|e| Error::Db(e.to_string()))?;
    let df = df
        .with_param_values(params)
        .map_err(|e| Error::Db(e.to_string()))?;
    let batches = df.collect().await.map_err(|e| Error::Db(e.to_string()))?;
    Ok(configs_from_batches(batches))
}

/// `.type()` clause for the `config` table: a string or array of
/// strings.
fn type_clause(spec: &json::Value, params: &mut Vec<ScalarValue>) -> crate::Result<String> {
    match spec {
        json::Value::String(s) => {
            params.push(ScalarValue::Utf8(Some(s.clone())));
            Ok(format!("\"type\" = ${}", params.len()))
        }
        json::Value::Array(items) => {
            let placeholders = crate::query::string_in_list(items, params, "type")?;
            Ok(format!("\"type\" IN ({placeholders})"))
        }
        _ => Err(Error::Args(
            "`.type()` expects a string or array of strings".into(),
        )),
    }
}

/// One `config` table row, minus `text`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Config {
    #[rune(get)]
    pub scope: String,
    #[rune(get)]
    pub project: String,
    #[rune(skip)]
    pub type_: String,
    #[rune(get)]
    pub name: String,
    #[rune(skip)]
    pub path: PathBuf,
    #[rune(get)]
    pub size: i64,
    #[rune(get)]
    pub mtime: DateTime,
}

/// Read the config file's contents.
#[rune::function(instance)]
async fn read(this: Ref<Config>) -> crate::Result<String> {
    let path = this.path.clone();
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))
}

impl Config {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> rune::alloc::Result<()> {
        write!(
            f,
            "Config {{ scope: {:?}, project: {:?}, type: {:?}, name: {:?}, \
             path: {:?}, size: {}, mtime: {} }}",
            self.scope,
            self.project,
            self.type_,
            self.name,
            self.path.display().to_string(),
            self.size,
            self.mtime.to_rfc3339(),
        )
    }
}

fn configs_from_batches(batches: Vec<RecordBatch>) -> Vec<Config> {
    let mut configs = Vec::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let schema = batch.schema();
        let str_col = |name: &str| {
            batch
                .column(schema.index_of(name).unwrap())
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
        };
        let scope_arr = str_col("scope");
        let project_arr = str_col("project");
        let type_arr = str_col("type");
        let name_arr = str_col("name");
        let path_arr = str_col("path");
        let size_arr = batch
            .column(schema.index_of("size").unwrap())
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let mtime_arr = batch
            .column(schema.index_of("mtime").unwrap())
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .unwrap();

        for row in 0..batch.num_rows() {
            configs.push(Config {
                scope: scope_arr.value(row).to_string(),
                project: project_arr.value(row).to_string(),
                type_: type_arr.value(row).to_string(),
                name: name_arr.value(row).to_string(),
                path: PathBuf::from(path_arr.value(row)),
                size: size_arr.value(row),
                mtime: DateTime::from_millis(mtime_arr.value(row)),
            });
        }
    }
    configs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: json::Value) -> json::Map<String, json::Value> {
        match v {
            json::Value::Object(m) => m,
            _ => panic!("test fixture must be a JSON object"),
        }
    }

    #[test]
    fn walk_returns_none_for_missing_path() {
        let m = obj(json!({"a": {"b": 1}}));
        assert!(walk_object_path(&m, "a.c").is_none());
        assert!(walk_object_path(&m, "missing").is_none());
    }

    #[test]
    fn walk_stops_at_scalar_mid_path() {
        let m = obj(json!({"a": 5}));
        assert!(walk_object_path(&m, "a.b").is_none());
    }

    #[test]
    fn walk_does_not_descend_into_arrays() {
        let m = obj(json!({"a": [1, 2, 3]}));
        assert!(walk_object_path(&m, "a.0").is_none());
    }
}
