//! Rune-facing `config()` accessor.
//!
//! Exposes the current task's Claude Code configuration as a `Config`
//! value with a `.settings()` query that resolves to `Settings`.
//!
//! Implementation note: the design doc frames this as a SQL query over
//! the `config` table, but the scheduler's per-task `df_ctx` registers
//! only `entry`/`message`. Rather than thread `config` table
//! registration through the scheduler for one trivial 2-column lookup,
//! we go straight to `gage_claude`'s discovery API. Same observable
//! behavior; no SQL machinery between scanner and disk.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use gage_claude::config::ConfigFile;
use gage_claude::home::ClaudeHome;
use rune::alloc;
use rune::runtime::{Object, Value};
use rune::{Any, ContextError, Module};
use serde_json as json;
use tracing::warn;

use crate::runtime::state::{TaskTarget, current_scan_ctx};
use crate::runtime::value::json_to_value;

/// Object-path segments listed here concatenate across scopes (user,
/// then project, then local) instead of being overridden. Source:
/// <https://code.claude.com/docs/en/settings#how-scopes-interact>.
const MERGED_ARRAY_PATHS: &[&str] = &[
    "permissions.allow",
    "permissions.deny",
    "permissions.ask",
    "allowedHttpHookUrls",
    "httpHookAllowedEnvVars",
    "sandbox.filesystem.allowRead",
    "sandbox.filesystem.denyRead",
    "sandbox.filesystem.allowWrite",
    "sandbox.filesystem.denyWrite",
    "sandbox.network.deniedDomains",
    "allowedMcpServers",
    "deniedMcpServers",
];

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("config", config).build()?;
    m.function_meta(Config::settings)?;
    m.associated_function(
        &rune::runtime::Protocol::INTO_FUTURE,
        |q: SettingsQuery| async move { fetch_settings(q).await },
    )?;
    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Config>()?;
    m.ty::<SettingsQuery>()?;
    m.ty::<Settings>()?;
    m.function_meta(Settings::get)?;
    m.function_meta(Settings::get_all)?;
    m.function_meta(Settings::get_scoped)?;
    Ok(())
}

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Config {
    #[rune(skip)]
    project: Option<PathBuf>,
}

impl Config {
    #[rune::function(instance)]
    fn settings(&self) -> SettingsQuery {
        SettingsQuery {
            project: self.project.clone(),
        }
    }
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct SettingsQuery {
    #[rune(skip)]
    project: Option<PathBuf>,
}

fn config() -> Config {
    let ctx = current_scan_ctx();
    let project = match &ctx.target {
        TaskTarget::Session { project, .. } => project.as_deref().map(|p| p.path.clone()),
        TaskTarget::Project(p) => Some(p.path.clone()),
        TaskTarget::Scan => None,
    };
    Config { project }
}

async fn fetch_settings(q: SettingsQuery) -> Settings {
    let mut settings = Settings::empty();

    let home = match ClaudeHome::from_env() {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "settings: cannot resolve Claude home");
            return settings;
        }
    };

    // User scope: walk only the settings phase
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
            Ok(ConfigFile::Settings(p)) => settings.user = load_settings(&p),
            Ok(_) => {}
            Err(e) => warn!(error = %e, scope = "user", "settings: walk error"),
        }
    }

    // Project scope: locate the project by path and walk settings +
    // local_settings phases only
    if let Some(target) = q.project {
        let projects = match home.projects() {
            Ok(ps) => ps,
            Err(e) => {
                warn!(error = %e, "settings: enumerate projects");
                Vec::new()
            }
        };
        let matched = projects.into_iter().find(|p| p.path == target);
        if let Some(project) = matched {
            let files = project
                .config()
                .memory(false)
                .skills(false)
                .commands(false)
                .agents(false)
                .find();
            for f in files {
                match f {
                    Ok(ConfigFile::Settings(p)) => settings.project = load_settings(&p),
                    Ok(ConfigFile::LocalSettings(p)) => settings.local = load_settings(&p),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, scope = "project", "settings: walk error"),
                }
            }
        }
    }

    settings
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
pub struct Settings {
    #[rune(skip)]
    user: Option<json::Map<String, json::Value>>,
    #[rune(skip)]
    project: Option<json::Map<String, json::Value>>,
    #[rune(skip)]
    local: Option<json::Map<String, json::Value>>,
    /// Memoized merged view, built on first `get()` call. Avoids
    /// redoing the fold on every lookup within a scanner.
    #[rune(skip)]
    merged: OnceLock<json::Map<String, json::Value>>,
}

impl Settings {
    fn empty() -> Self {
        Self {
            user: None,
            project: None,
            local: None,
            merged: OnceLock::new(),
        }
    }

    fn merged(&self) -> &json::Map<String, json::Value> {
        self.merged.get_or_init(|| {
            let mut acc = json::Map::new();
            for overlay in [&self.user, &self.project, &self.local]
                .into_iter()
                .flatten()
            {
                merge_into(&mut acc, overlay, "");
            }
            acc
        })
    }

    /// Resolve a dot-delimited object path against the fully merged
    /// settings view. Returns `None` if any path segment misses or hits
    /// a non-object before the final segment.
    #[rune::function(instance)]
    fn get(&self, path: String) -> Option<Value> {
        let merged = self.merged();
        let value = walk_object_path(merged, &path)?;
        Some(json_to_value(value))
    }

    /// One entry per scope that has *some* value at `path` (no merging
    /// applied). Entries are `#{value, scope}` objects in order
    /// `local`, `project`, `user` — highest priority first.
    #[rune::function(instance)]
    fn get_all(&self, path: String) -> Vec<Value> {
        let mut out = Vec::new();
        for (label, scope) in [
            ("local", &self.local),
            ("project", &self.project),
            ("user", &self.user),
        ] {
            let Some(map) = scope else { continue };
            let Some(value) = walk_object_path(map, &path) else {
                continue;
            };
            let mut entry = Object::new();
            entry
                .insert(
                    alloc::String::try_from("value").unwrap(),
                    json_to_value(value),
                )
                .unwrap();
            entry
                .insert(
                    alloc::String::try_from("scope").unwrap(),
                    rune::to_value(alloc::String::try_from(label).unwrap()).unwrap(),
                )
                .unwrap();
            out.push(rune::to_value(entry).unwrap());
        }
        out
    }

    /// Resolve `path` against one named scope only (`"user"`,
    /// `"project"`, or `"local"`). Unknown scope name → `None` with a
    /// warning.
    #[rune::function(instance)]
    fn get_scoped(&self, scope: String, path: String) -> Option<Value> {
        let map = match scope.as_str() {
            "user" => self.user.as_ref(),
            "project" => self.project.as_ref(),
            "local" => self.local.as_ref(),
            other => {
                warn!(scope = %other, "settings: get_scoped called with unknown scope");
                return None;
            }
        }?;
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

/// Fold `overlay` into `base` in-place. `prefix` is the dotted path
/// `base` represents (empty at the root) so nested calls can match
/// `MERGED_ARRAY_PATHS`.
fn merge_into(
    base: &mut json::Map<String, json::Value>,
    overlay: &json::Map<String, json::Value>,
    prefix: &str,
) {
    for (k, v) in overlay {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        let existing = base.get_mut(k);
        match (existing, v) {
            (Some(json::Value::Object(eo)), json::Value::Object(vo)) => {
                merge_into(eo, vo, &path);
            }
            (Some(json::Value::Array(ea)), json::Value::Array(va))
                if MERGED_ARRAY_PATHS.iter().any(|p| *p == path) =>
            {
                ea.extend(va.iter().cloned());
            }
            (_, fresh) => {
                base.insert(k.clone(), fresh.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: json::Value) -> Option<json::Map<String, json::Value>> {
        match v {
            json::Value::Object(m) => Some(m),
            _ => panic!("test fixture must be a JSON object"),
        }
    }

    fn build(
        user: Option<json::Value>,
        project: Option<json::Value>,
        local: Option<json::Value>,
    ) -> Settings {
        Settings {
            user: user.and_then(obj),
            project: project.and_then(obj),
            local: local.and_then(obj),
            merged: OnceLock::new(),
        }
    }

    #[test]
    fn scope_precedence_simple() {
        let s = build(
            Some(json!({"effortLevel": "low"})),
            Some(json!({"effortLevel": "mid"})),
            Some(json!({"effortLevel": "high"})),
        );
        let m = s.merged();
        assert_eq!(m.get("effortLevel").unwrap(), &json!("high"));
    }

    #[test]
    fn objects_merge_recursively() {
        let s = build(
            Some(json!({"a": {"x": 1, "y": 2}})),
            Some(json!({"a": {"y": 20, "z": 30}})),
            None,
        );
        let m = s.merged();
        assert_eq!(m.get("a").unwrap(), &json!({"x": 1, "y": 20, "z": 30}));
    }

    #[test]
    fn merged_array_path_concats() {
        let s = build(
            Some(json!({"permissions": {"allow": ["WebSearch"]}})),
            Some(json!({"permissions": {"allow": ["Bash(ls)"]}})),
            Some(json!({"permissions": {"deny": ["Bash(rm)"], "allow": ["Skill(x)"]}})),
        );
        let m = s.merged();
        assert_eq!(
            m.get("permissions").unwrap(),
            &json!({
                "allow": ["WebSearch", "Bash(ls)", "Skill(x)"],
                "deny": ["Bash(rm)"],
            }),
        );
    }

    #[test]
    fn non_listed_array_path_overrides() {
        // `permissions.foo` is *not* in MERGED_ARRAY_PATHS, so local
        // wins outright.
        let s = build(
            Some(json!({"permissions": {"foo": [1, 2]}})),
            None,
            Some(json!({"permissions": {"foo": [9]}})),
        );
        let m = s.merged();
        assert_eq!(m.get("permissions").unwrap(), &json!({"foo": [9]}));
    }

    #[test]
    fn walk_returns_none_for_missing_path() {
        let m = obj(json!({"a": {"b": 1}})).unwrap();
        assert!(walk_object_path(&m, "a.c").is_none());
        assert!(walk_object_path(&m, "missing").is_none());
    }

    #[test]
    fn walk_stops_at_scalar_mid_path() {
        // `a` is a scalar; `a.b` must not descend into it.
        let m = obj(json!({"a": 5})).unwrap();
        assert!(walk_object_path(&m, "a.b").is_none());
    }

    #[test]
    fn walk_does_not_descend_into_arrays() {
        let m = obj(json!({"a": [1, 2, 3]})).unwrap();
        assert!(walk_object_path(&m, "a.0").is_none());
    }

    #[test]
    fn scanner_rn_test_settings_fixture() {
        // Mirrors test_settings() in docs/design/.../scanner.rn: the
        // canonical example documenting expected merge behavior.
        let s = build(
            Some(json!({
                "permissions": {"allow": ["WebSearch", "WebFetch"]},
                "effortLevel": "low",
            })),
            Some(json!({
                "permissions": {"allow": ["Bash(gh search:*)", "Skill(review-opened-file:*)", "Skill(rune:*)"]},
            })),
            Some(json!({
                "effortLevel": "high",
                "permissions": {"deny": ["Bash(rm -rf:*)"]},
            })),
        );
        let m = s.merged();
        assert_eq!(m.get("effortLevel").unwrap(), &json!("high"));
        assert_eq!(
            m.get("permissions").unwrap(),
            &json!({
                "allow": [
                    "WebSearch", "WebFetch",
                    "Bash(gh search:*)", "Skill(review-opened-file:*)", "Skill(rune:*)",
                ],
                "deny": ["Bash(rm -rf:*)"],
            }),
        );
    }
}
