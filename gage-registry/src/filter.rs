//! The publish filter registry: the embedded `filters/` bundle, the
//! roots a filter is discovered under, the `FILTER` declaration, and
//! the resolution of a filter's params. The layout follows the
//! scanner registry: the builtin bundle is extracted to
//! `<gage_home>/lib/filters`, a user's own filters live under
//! `<gage_home>/local/filters`, and a project's under `.gage/filters`,
//! most specific first.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use gage_core::config::FilterConfig;
use rune::SourceId;
use rune::ast::{self, Spanned};
use rune::parse;
use rune::runtime::Object;
use rust_embed::Embed;
use serde_json as json;

use crate::DefSource;
use crate::scanner::{
    ParseError, Problem, apply_params, const_field, discover_project_dirs, expr_str, expr_str_vec,
    extract_embedded, field_key, function_name, parse_object_repr, parse_params_def, problem,
    render_problems, resolve_params, walk_rn_files,
};

#[derive(Embed)]
#[folder = "../filters/"]
struct Filters;

pub fn filters_dir() -> PathBuf {
    gage_core::config::gage_home().join("lib/filters")
}

/// User-level custom filter root: `<gage_home>/local/filters`. The
/// embedded extraction never touches it.
pub fn user_filters_dir() -> PathBuf {
    gage_core::config::gage_home().join("local/filters")
}

/// Classify a filter file by its filesystem location, the way
/// [`crate::scanner::scanner_source`] classifies a scanner file.
pub fn filter_source(path: &Path) -> DefSource {
    if path.starts_with(filters_dir()) {
        DefSource::Builtin
    } else if path.starts_with(user_filters_dir()) {
        DefSource::User
    } else {
        DefSource::Project
    }
}

/// The filter roots, most specific first: each project `.gage/filters`
/// walking up from the current directory, then the user root, then
/// the builtin bundle.
pub fn filter_home_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        out.extend(discover_project_dirs(&cwd, "filters"));
    }
    out.push(user_filters_dir());
    out.push(filters_dir());
    out
}

/// Extract the embedded filter bundle to [`filters_dir`].
pub fn extract_filters() -> std::io::Result<()> {
    let dir = filters_dir();
    assert!(dir.ends_with("lib/filters"));
    extract_embedded::<Filters>(&dir)
}

/// One task a filter declares: the function the publish calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterTaskDef {
    pub name: String,
    /// The function that runs the task: `call` when given, else the
    /// task name with `-` as `_`
    pub call: String,
}

/// A filter as its `FILTER` declaration states it.
pub struct FilterDef {
    pub name: String,
    pub description: String,
    /// The semver `FILTER.version`, as declared; empty when the
    /// declaration has none. A publish records it beside the filter's
    /// name, and a share's version requirement is checked against it.
    pub version: String,
    /// The integer `FILTER.generation`; 0 when the declaration has
    /// none. A filter raises it when sessions published under a lower
    /// generation hold content the filter now removes. A publish
    /// records it, and a batch publish replaces a published session
    /// whose recorded generation is lower.
    pub generation: u32,
    /// Group memberships declared via `FILTER.groups`; `-g --group`
    /// selects by any group name
    pub groups: Vec<String>,
    pub tasks: BTreeMap<String, FilterTaskDef>,
    /// Defects in the `FILTER` declaration, in source order. A def
    /// with problems is not fit to compile.
    pub problems: Vec<Problem>,
    ast: ast::File,
    source: String,
    pub embed_key: String,
    /// Absolute filesystem path to the filter's `.rn` source file
    pub path: PathBuf,
}

impl FilterDef {
    pub fn params_def(&self) -> Object {
        match const_field(&self.ast, &self.source, "FILTER", "params") {
            Some(ast::Expr::Object(obj)) => parse_params_def(&self.source, obj),
            _ => Object::new(),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The problems as one codespan report, each labelled at its
    /// source span, for the compile error that stops the filter.
    pub fn render_problems(&self) -> String {
        render_problems(&self.embed_key, &self.source, &self.path, &self.problems)
    }
}

impl fmt::Debug for FilterDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterDef")
            .field("name", &self.name)
            .field("groups", &self.groups)
            .field("path", &self.path)
            .finish()
    }
}

/// A filter resolved for a publish: its def and its params.
pub struct Filter<'a> {
    pub def: &'a FilterDef,
    pub params: Option<json::Value>,
}

impl fmt::Debug for Filter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Filter")
            .field("name", &self.def.name)
            .field("params", &self.params)
            .finish()
    }
}

#[derive(Debug)]
pub enum FilterSpecError {
    Params(String, String),
}

impl fmt::Display for FilterSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterSpecError::Params(spec, msg) => {
                write!(f, "Invalid filter params '{spec}': {msg}")
            }
        }
    }
}

impl std::error::Error for FilterSpecError {}

impl<'a> Filter<'a> {
    /// The filter `def` with its params resolved in layers: the
    /// declared default, then the config's `[filters.<name>.params]`,
    /// then the `#{...}` override.
    pub fn from_spec(
        def: &'a FilterDef,
        config: &FilterConfig,
        params_override: Option<&str>,
        spec_for_diagnostics: &str,
    ) -> Result<Self, FilterSpecError> {
        let mut params = resolve_params(&def.params_def());
        if let Some(configured) = config.params(&def.name) {
            apply_params(&mut params, configured.clone(), &def.name, "config");
        }
        if let Some(override_src) = params_override {
            let overrides = parse_object_repr(override_src)
                .map_err(|e| FilterSpecError::Params(spec_for_diagnostics.to_string(), e))?;
            apply_params(&mut params, overrides, &def.name, "override");
        }
        Ok(Filter { def, params })
    }

    /// The filter `def` with its params as the publish definition
    /// recorded them: the declared defaults with `recorded` over them.
    pub fn with_params(
        def: &'a FilterDef,
        recorded: Option<&json::Map<String, json::Value>>,
    ) -> Self {
        let mut params = resolve_params(&def.params_def());
        if let Some(recorded) = recorded {
            apply_params(&mut params, recorded.clone(), &def.name, "definition");
        }
        Filter { def, params }
    }
}

pub struct FilterRegistry {
    defs: Vec<FilterDef>,
    errors: Vec<(PathBuf, String)>,
    /// Declared name to ordered indices into `defs`, highest
    /// precedence first; the rest are shadowed
    by_name: HashMap<String, Vec<usize>>,
}

impl FilterRegistry {
    pub fn load() -> Self {
        extract_filters().unwrap();
        Self::load_from_roots(&filters_dir(), filter_home_paths())
    }

    /// Load filters from an ordered list of roots, most specific
    /// first. Files under `builtin` use a relative embed key; files
    /// under any other root use an absolute one.
    pub fn load_from_roots(builtin: &Path, roots: Vec<PathBuf>) -> Self {
        let mut defs: Vec<FilterDef> = Vec::new();
        let mut errors: Vec<(PathBuf, String)> = Vec::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for root in roots {
            for path in walk_rn_files(&root) {
                let key = if root.as_path() == builtin {
                    path.strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                } else {
                    path.to_string_lossy().to_string()
                };
                let code = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("{key}: {e}");
                        continue;
                    }
                };
                match parse_filter(&code, &key, &path) {
                    Ok(def) => {
                        let idx = defs.len();
                        let name = def.name.clone();
                        defs.push(def);
                        by_name.entry(name).or_default().push(idx);
                    }
                    Err(ParseError::MissingConst(_)) => {}
                    Err(ref e) => errors.push((path.clone(), e.render(&key, &code))),
                }
            }
        }
        for (name, indices) in &by_name {
            if indices.len() > 1 {
                let paths: Vec<String> = indices
                    .iter()
                    .map(|&i| {
                        defs.get(i)
                            .expect("by_name index came from defs.len() at push time")
                            .path
                            .display()
                            .to_string()
                    })
                    .collect();
                let (winning, shadowed) = paths
                    .split_first()
                    .expect("indices.len() > 1 guarantees paths non-empty");
                tracing::warn!(
                    filter = %name,
                    winning = %winning,
                    shadowed = ?shadowed,
                    "multiple filters declare the same name; higher-precedence root wins"
                );
            }
        }
        FilterRegistry {
            defs,
            errors,
            by_name,
        }
    }

    pub fn get_def(&self, name: &str) -> Option<&FilterDef> {
        let idx = *self.by_name.get(name)?.first()?;
        Some(
            self.defs
                .get(idx)
                .expect("by_name index came from defs.len() at push time"),
        )
    }

    /// True if `name` maps to a parsed filter in the registry.
    pub fn is_known(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Every parse failure found during load, as `(path, rendered
    /// error)` pairs.
    pub fn parse_errors(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.errors.iter().map(|(p, e)| (p.as_path(), e.as_str()))
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.by_name.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// The winning def of every name, sorted by name.
    pub fn list(&self) -> Vec<&FilterDef> {
        let mut defs: Vec<&FilterDef> = self
            .by_name
            .values()
            .map(|indices| {
                let idx = *indices
                    .first()
                    .expect("by_name entry has >= 1 index at push time");
                self.defs
                    .get(idx)
                    .expect("by_name index came from defs.len() at push time")
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Filters declaring membership in `group`, sorted by name.
    pub fn group_members(&self, group: &str) -> Vec<&FilterDef> {
        self.list()
            .into_iter()
            .filter(|d| d.groups.iter().any(|g| g == group))
            .collect()
    }

    /// Every group any filter declares, sorted.
    pub fn groups(&self) -> Vec<&str> {
        let mut groups: Vec<&str> = self
            .list()
            .into_iter()
            .flat_map(|d| d.groups.iter().map(String::as_str))
            .collect();
        groups.sort_unstable();
        groups.dedup();
        groups
    }
}

fn parse_filter(source: &str, embed_key: &str, path: &Path) -> Result<FilterDef, ParseError> {
    let file = parse::parse_all::<ast::File>(source, SourceId::empty(), false)
        .map_err(ParseError::Syntax)?;
    let mut name = None;
    let mut description = None;
    let mut version = String::new();
    let mut generation = 0;
    let mut groups: Vec<String> = Vec::new();
    let mut tasks_obj: Option<&ast::ExprObject> = None;
    let mut problems: Vec<Problem> = Vec::new();

    for (item, _) in &file.items {
        let ast::Item::Const(item_const) = item else {
            continue;
        };
        let span = item_const.name.span();
        if &source[span.start.0 as usize..span.end.0 as usize] != "FILTER" {
            continue;
        }
        let ast::Expr::Object(obj) = &item_const.expr else {
            continue;
        };
        for (field, _) in &obj.assignments {
            let key = field_key(source, &field.key);
            let Some((_, expr)) = &field.assign else {
                continue;
            };
            match key.as_deref() {
                Some("name") => name = expr_str(source, expr),
                Some("description") => {
                    description = expr_str(source, expr).and_then(|s| {
                        s.lines()
                            .find(|l| !l.trim().is_empty())
                            .map(|l| l.trim().to_string())
                    })
                }
                Some("version") => version = expr_str(source, expr).unwrap_or_default(),
                Some("generation") => match expr_u32(source, expr) {
                    Some(n) => generation = n,
                    None => problems.push(problem(
                        expr,
                        "FILTER field 'generation' must be a non-negative integer",
                    )),
                },
                Some("groups") => groups = expr_str_vec(source, expr),
                Some("tasks") => match expr {
                    ast::Expr::Object(obj) => tasks_obj = Some(obj),
                    _ => problems.push(problem(expr, "FILTER field 'tasks' must be an object")),
                },
                _ => {}
            }
        }
    }

    let Some(name) = name else {
        return Err(ParseError::MissingConst("FILTER"));
    };
    let tasks = match tasks_obj {
        Some(obj) => parse_tasks(source, obj, &mut problems),
        None => BTreeMap::new(),
    };
    problems.sort_by_key(|p| p.start);
    Ok(FilterDef {
        name,
        description: description.unwrap_or_default(),
        version,
        generation,
        groups,
        tasks,
        problems,
        ast: file,
        source: source.to_string(),
        embed_key: embed_key.to_string(),
        path: path.to_path_buf(),
    })
}

/// The value of an unsigned integer literal.
fn expr_u32(source: &str, expr: &ast::Expr) -> Option<u32> {
    let span = expr.span();
    source[span.start.0 as usize..span.end.0 as usize]
        .parse()
        .ok()
}

/// Parse `FILTER.tasks`: each task is an object with an optional
/// `call`. A task of the wrong shape or a duplicate is a problem and
/// is left out.
fn parse_tasks(
    source: &str,
    obj: &ast::ExprObject,
    problems: &mut Vec<Problem>,
) -> BTreeMap<String, FilterTaskDef> {
    let mut tasks: BTreeMap<String, FilterTaskDef> = BTreeMap::new();
    for (field, _) in &obj.assignments {
        let Some(name) = field_key(source, &field.key) else {
            continue;
        };
        let Some((_, expr)) = &field.assign else {
            continue;
        };
        let ast::Expr::Object(task_obj) = expr else {
            problems.push(problem(expr, format!("task '{name}' must be an object")));
            continue;
        };
        if tasks.contains_key(&name) {
            problems.push(problem(expr, format!("duplicate task '{name}'")));
            continue;
        }
        let mut call = function_name(&name);
        for (tf, _) in &task_obj.assignments {
            let Some(tkey) = field_key(source, &tf.key) else {
                continue;
            };
            let Some((_, texpr)) = &tf.assign else {
                continue;
            };
            if tkey == "call" {
                match expr_str(source, texpr) {
                    Some(c) => call = c,
                    None => problems.push(problem(
                        texpr,
                        format!("task '{name}' field 'call' has unexpected type"),
                    )),
                }
            }
        }
        tasks.insert(name.clone(), FilterTaskDef { name, call });
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filter_reads_the_declaration_and_defaults_each_call() {
        let source = r#"
use gage::publish;

pub const FILTER = #{
    name: "demo",
    description: "Remove demo content",
    version: "1.2.0",
    generation: 3,
    groups: ["public", "trusted"],
    params: #{ prompt: #{ value: "", description: "Additional instructions" }, n: 2 },
    tasks: #{ "demo-task": #{}, other: #{ call: "run_other" } },
};
"#;
        let def = parse_filter(source, "demo", Path::new("/tmp/demo/filter.rn")).unwrap();
        assert!(def.problems.is_empty(), "{:?}", def.problems);
        assert_eq!(def.name, "demo");
        assert_eq!(def.description, "Remove demo content");
        assert_eq!(def.version, "1.2.0");
        assert_eq!(def.generation, 3);
        assert_eq!(def.groups, ["public", "trusted"]);
        assert_eq!(def.tasks["demo-task"].call, "demo_task");
        assert_eq!(def.tasks["other"].call, "run_other");
        let params = resolve_params(&def.params_def()).unwrap();
        assert_eq!(params, json::json!({ "prompt": "", "n": 2 }));
    }

    /// A declaration without a generation is generation 0, and a
    /// generation that is not an unsigned integer is a problem.
    #[test]
    fn generation_defaults_to_zero_and_rejects_a_non_integer() {
        let path = Path::new("/tmp/demo/filter.rn");
        let def = parse_filter("pub const FILTER = #{ name: \"demo\" };", "demo", path).unwrap();
        assert_eq!(def.generation, 0);
        let def = parse_filter(
            "pub const FILTER = #{ name: \"demo\", generation: \"2\" };",
            "demo",
            path,
        )
        .unwrap();
        assert_eq!(def.generation, 0);
        assert!(
            def.problems[0].message.contains("'generation'"),
            "{:?}",
            def.problems
        );
    }

    #[test]
    fn a_file_without_a_filter_constant_is_not_a_filter() {
        let err = parse_filter(
            "pub const SCANNER = #{ name: \"s\" };",
            "s",
            Path::new("/tmp/s/scanner.rn"),
        )
        .unwrap_err();
        assert!(matches!(err, ParseError::MissingConst("FILTER")), "{err}");
        assert_eq!(err.to_string(), "missing FILTER constant");
    }

    #[test]
    fn declaration_defects_are_problems_on_the_def() {
        let source = r#"
pub const FILTER = #{
    name: "demo",
    tasks: #{
        main: #{ call: run },
        main: #{},
        text: "nope",
    },
};
"#;
        let def = parse_filter(source, "demo", Path::new("/tmp/demo/filter.rn")).unwrap();
        let messages: Vec<&str> = def.problems.iter().map(|p| p.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "task 'main' field 'call' has unexpected type",
                "duplicate task 'main'",
                "task 'text' must be an object",
            ]
        );
        assert_eq!(def.tasks.keys().collect::<Vec<_>>(), ["main"]);
        assert!(def.render_problems().contains("duplicate task 'main'"));
    }

    #[test]
    fn from_spec_layers_config_params_under_the_override() {
        let source = r#"
pub const FILTER = #{
    name: "demo",
    params: #{ prompt: #{ value: "" }, mode: "strict" },
};
"#;
        let def = parse_filter(source, "demo", Path::new("/tmp/demo/filter.rn")).unwrap();
        let config: FilterConfig = toml::from_str(
            r#"
[demo.params]
prompt = "Watch for hosts."
unknown = 1
"#,
        )
        .unwrap();
        let configured = Filter::from_spec(&def, &config, None, "demo").unwrap();
        assert_eq!(
            configured.params.unwrap(),
            json::json!({ "prompt": "Watch for hosts.", "mode": "strict" })
        );
        let overridden =
            Filter::from_spec(&def, &config, Some(r#"#{ mode: "loose" }"#), "demo").unwrap();
        assert_eq!(
            overridden.params.unwrap(),
            json::json!({ "prompt": "Watch for hosts.", "mode": "loose" })
        );
        let err = Filter::from_spec(&def, &config, Some("#{ mode: "), "demo#{ mode: ")
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Invalid filter params 'demo#{ mode: '"),
            "{err}"
        );
        let recorded = json::json!({ "prompt": "From the file." });
        let from_file = Filter::with_params(&def, recorded.as_object());
        assert_eq!(
            from_file.params.unwrap(),
            json::json!({ "prompt": "From the file.", "mode": "strict" })
        );
    }

    fn write_filter(dir: &Path, subdir: &str, name: &str, groups: &str) {
        let bundle = dir.join(subdir);
        std::fs::create_dir_all(&bundle).unwrap();
        let source = format!(
            "pub const FILTER = #{{ name: \"{name}\", description: \"{subdir}\", groups: {groups} }};\n"
        );
        std::fs::write(bundle.join("filter.rn"), source).unwrap();
    }

    #[test]
    fn load_from_roots_shadows_by_precedence_and_lists_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let builtin = tmp.path().join("builtin");
        let user = tmp.path().join("user");
        write_filter(&builtin, "shared", "shared", "[\"public\"]");
        write_filter(
            &builtin,
            "only-builtin",
            "only-builtin",
            "[\"public\", \"reading\"]",
        );
        write_filter(&user, "shared-user", "shared", "[\"trusted\"]");
        std::fs::write(user.join("broken.rn"), "not valid rune!!!").unwrap();
        std::fs::write(
            user.join("scanner.rn"),
            "pub const SCANNER = #{ name: \"s\" };",
        )
        .unwrap();

        let reg = FilterRegistry::load_from_roots(&builtin, vec![user.clone(), builtin.clone()]);
        assert_eq!(reg.get_def("shared").unwrap().description, "shared-user");
        assert_eq!(reg.names(), ["only-builtin", "shared"]);
        assert!(reg.is_known("only-builtin") && !reg.is_known("nope"));
        assert_eq!(reg.groups(), ["public", "reading", "trusted"]);
        let public: Vec<&str> = reg
            .group_members("public")
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(public, ["only-builtin"]);
        let errors: Vec<PathBuf> = reg.parse_errors().map(|(p, _)| p.to_path_buf()).collect();
        assert_eq!(errors, [user.join("broken.rn")]);
        assert!(!Path::new(&reg.get_def("only-builtin").unwrap().embed_key).is_absolute());
        assert!(Path::new(&reg.get_def("shared").unwrap().embed_key).is_absolute());
    }
}
