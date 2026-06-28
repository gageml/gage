//! Rune-facing `user().config()` accessor.
//!
//! Exposes Claude Code's user-level configuration (`~/.claude/...`) as
//! a `Config` value resolved asynchronously, matching Claude Code's
//! "user settings" terminology. There is no project-level access today;
//! when one is needed, a `project(p).config()` parallel form fits the
//! same shape.
//!
//! Goes straight to `gage_claude`'s discovery API — no SQL machinery
//! between scanner and disk for a 2-column lookup.

use std::path::Path;

use gage_claude::config::ConfigFile;
use gage_claude::home::ClaudeHome;
use rune::runtime::Value;
use rune::{Any, ContextError, Module};
use serde_json as json;
use tracing::warn;

use crate::value::json_to_value;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("user", user).build()?;
    m.function_meta(User::config)?;
    m.associated_function(
        &rune::runtime::Protocol::INTO_FUTURE,
        |q: ConfigQuery| async move { fetch_config(q).await },
    )?;
    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<User>()?;
    m.ty::<ConfigQuery>()?;
    m.ty::<Config>()?;
    m.function_meta(Config::get)?;
    Ok(())
}

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct User {}

impl User {
    #[rune::function(instance)]
    fn config(&self) -> ConfigQuery {
        ConfigQuery {}
    }
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct ConfigQuery {}

fn user() -> User {
    User {}
}

async fn fetch_config(_q: ConfigQuery) -> Config {
    let mut config = Config::empty();

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
pub struct Config {
    #[rune(skip)]
    settings: Option<json::Map<String, json::Value>>,
}

impl Config {
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
