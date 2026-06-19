use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use rune::SourceId;
use rune::alloc;
use rune::alloc::prelude::TryClone;
use rune::ast::{self, Spanned};
use rune::parse;
use rune::runtime::{Object, Value};
use rust_embed::Embed;
use serde::Serialize;
use serde_json as json;

#[derive(Embed)]
#[folder = "../scanners/"]
struct Scanners;

pub fn scanners_dir() -> PathBuf {
    gage_core::config::gage_home().join("lib/scanners")
}

/// Ordered list of "scanner home" directories searched when resolving
/// an absolute `scanner:/…` URI. Today this is a single-element list;
/// future revisions will let users configure additional roots.
pub fn scanner_home_paths() -> Vec<PathBuf> {
    vec![scanners_dir()]
}

pub fn extract_scanners() -> std::io::Result<()> {
    let dir = scanners_dir();
    for path in Scanners::iter() {
        let file = Scanners::get(&path).expect("embedded key exists");
        let target = dir.join(path.as_ref());
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &file.data)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskNotesDef {
    pub wants: Vec<String>,
    pub writes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDef {
    pub name: String,
    pub notes: TaskNotesDef,
}

pub struct ScannerDef {
    pub name: String,
    pub description: String,
    pub version: String,
    pub hidden: bool,
    pub tasks: BTreeMap<String, TaskDef>,
    ast: ast::File,
    source: String,
    pub(crate) embed_key: String,
    /// True for scanners loaded ad hoc via `-f / --file`. These are
    /// excluded from listings (`list_visible` / `list_enabled`) and
    /// from enable/disable settings management.
    pub from_file: bool,
}

impl ScannerDef {
    pub fn config(&self) -> Object {
        match self.scanner_field("config") {
            Some(ast::Expr::Object(obj)) => parse_config(&self.source, obj),
            _ => Object::new(),
        }
    }

    pub fn config_json(&self) -> Option<json::Value> {
        let config = self.config();
        if config.is_empty() {
            return None;
        }
        let rune_val = rune::to_value(config).unwrap();
        Some(json::to_value(&rune_val).unwrap())
    }

    /// Filesystem directory containing this scanner's `.rn` source.
    /// Used as the resolution root for relative `scanner:…` URIs.
    pub fn module_dir(&self) -> PathBuf {
        if self.from_file {
            return PathBuf::from(&self.embed_key)
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));
        }

        let rel = self
            .embed_key
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("");
        scanners_dir().join(rel)
    }

    fn scanner_field(&self, name: &str) -> Option<&ast::Expr> {
        for (item, _) in &self.ast.items {
            let ast::Item::Const(item_const) = item else {
                continue;
            };
            if self.ident(item_const.name.span()) != "SCANNER" {
                continue;
            }
            let ast::Expr::Object(obj) = &item_const.expr else {
                continue;
            };
            for (field, _) in &obj.assignments {
                if field_key(&self.source, &field.key).as_deref() == Some(name) {
                    return field.assign.as_ref().map(|(_, expr)| expr);
                }
            }
        }
        None
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    fn ident(&self, span: rune::ast::Span) -> &str {
        &self.source[span.start.0 as usize..span.end.0 as usize]
    }
}

#[derive(Debug)]
pub enum ParseError {
    Syntax(rune::compile::Error),
    MissingScanner,
    MissingName,
    DuplicateTask(String),
    TaskFieldType {
        task: String,
        field: &'static str,
    },
    IncludeStr {
        task: String,
        note: String,
        path: PathBuf,
        message: String,
    },
}

impl ParseError {
    pub fn location(&self, source: &str) -> String {
        match self {
            ParseError::Syntax(e) => {
                let offset = e.span().start.0 as usize;
                let line = source[..offset].matches('\n').count() + 1;
                let line_start = source[..offset].rfind('\n').map_or(0, |p| p + 1);
                let col = offset - line_start + 1;
                format!("{line}:{col}")
            }
            _ => "0:0".to_string(),
        }
    }

    /// Render this parse error with a source snippet (codespan-style),
    /// suitable for direct display. For `Syntax` errors the span drives
    /// a labeled diagnostic; other variants render as a plain message
    /// with the file:loc prefix.
    pub fn render(&self, embed_key: &str, source: &str) -> String {
        use codespan_reporting::diagnostic::{Diagnostic, Label};
        use codespan_reporting::term;
        use rune::{Source, Sources};

        let loc = self.location(source);

        match self {
            ParseError::Syntax(e) => {
                let mut sources = Sources::new();
                let source_id = sources
                    .insert(Source::with_path(embed_key, source, Path::new(embed_key)).unwrap())
                    .unwrap();
                let msg = format!("{e}");
                let diagnostic = Diagnostic::error().with_message(&msg).with_labels(vec![
                    Label::primary(source_id, e.span().range()).with_message(&msg),
                ]);
                let mut buf = rune::termcolor::Buffer::ansi();
                let config = term::Config::default();
                term::emit_to_write_style(&mut buf, &config, &sources, &diagnostic).unwrap();
                String::from_utf8(buf.into_inner()).unwrap()
            }
            _ => format!("error: {embed_key}:{loc} {self}\n"),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Syntax(e) => write!(f, "{e}"),
            ParseError::MissingScanner => write!(f, "missing SCANNER constant"),
            ParseError::MissingName => write!(f, "SCANNER missing 'name' field"),
            ParseError::DuplicateTask(name) => {
                write!(f, "duplicate task '{name}'")
            }
            ParseError::TaskFieldType { task, field } => {
                write!(f, "task '{task}' field '{field}' has unexpected type")
            }
            ParseError::IncludeStr {
                task,
                note,
                path,
                message,
            } => {
                write!(
                    f,
                    "task '{task}' note '{note}' include_str!({}): {message}",
                    path.display()
                )
            }
        }
    }
}

pub struct Scanner<'a> {
    pub def: &'a ScannerDef,
    pub params: Option<json::Value>,
}

impl fmt::Debug for Scanner<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanner")
            .field("name", &self.def.name)
            .field("params", &self.params)
            .finish()
    }
}

#[derive(Debug)]
pub enum ScannerSpecError {
    Name(String),
    ParseFailed(String, String),
    Config(String, String),
}

impl fmt::Display for ScannerSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScannerSpecError::Name(name) => write!(f, "Unknown scanner: {name}"),
            ScannerSpecError::ParseFailed(name, rendered) => {
                write!(f, "scanner '{name}' failed to parse:\n{rendered}")
            }
            ScannerSpecError::Config(spec, msg) => {
                write!(f, "Invalid scanner config '{spec}': {msg}")
            }
        }
    }
}

impl<'a> Scanner<'a> {
    pub fn from_spec(spec: &str, registry: &'a ScannerRegistry) -> Result<Self, ScannerSpecError> {
        let (name, config_override) = match spec.find("#{") {
            Some(pos) => (&spec[..pos], Some(&spec[pos..])),
            None => (spec, None),
        };

        let def = match registry.get_def(name) {
            Some(def) => def,
            None => {
                if let Some(rendered) = registry.parse_error(name) {
                    return Err(ScannerSpecError::ParseFailed(
                        name.to_string(),
                        rendered.to_string(),
                    ));
                }
                return Err(ScannerSpecError::Name(name.to_string()));
            }
        };

        let mut params = resolve_params(&def.config());

        if let Some(override_src) = config_override {
            let overrides = parse_object_repr(override_src)
                .map_err(|e| ScannerSpecError::Config(spec.to_string(), e))?;

            if let Some(ref mut params_json) = params {
                let map = params_json.as_object_mut().unwrap();
                for (key, val) in overrides {
                    if map.contains_key(&key) {
                        map.insert(key, val);
                    } else {
                        tracing::warn!(
                            scanner = name,
                            key,
                            "ignoring unknown config key in override"
                        );
                    }
                }
            } else if !overrides.is_empty() {
                let keys: Vec<_> = overrides.keys().collect();
                tracing::warn!(
                    scanner = name,
                    ?keys,
                    "scanner has no config; ignoring overrides"
                );
            }
        }

        Ok(Scanner { def, params })
    }
}

fn parse_object_repr(source: &str) -> Result<json::Map<String, json::Value>, String> {
    let expr = parse::parse_all::<ast::Expr>(source, SourceId::empty(), false)
        .map_err(|e| format!("syntax error: {e}"))?;
    let ast::Expr::Object(obj) = &expr else {
        return Err("expected object literal (#{{...}})".to_string());
    };
    let mut map = json::Map::new();
    for (field, _) in &obj.assignments {
        let Some(key) = field_key(source, &field.key) else {
            continue;
        };
        let Some((_, val_expr)) = &field.assign else {
            continue;
        };
        let Some(val) = expr_to_value(source, val_expr) else {
            let span = val_expr.span();
            let raw = &source[span.start.0 as usize..span.end.0 as usize];
            return Err(format!("unsupported value for '{key}': {raw}"));
        };
        let json_val =
            json::to_value(&val).map_err(|e| format!("cannot serialize '{key}': {e}"))?;
        map.insert(key, json_val);
    }
    Ok(map)
}

fn resolve_params(config: &Object) -> Option<json::Value> {
    if config.is_empty() {
        return None;
    }
    let mut params = Object::new();
    for (key, val) in config.iter() {
        if let Ok(entry) = val.borrow_ref::<Object>()
            && let Some(default) = entry.get("value")
        {
            params
                .insert(key.try_clone().unwrap(), default.clone())
                .unwrap();
        }
    }
    let rune_val = rune::to_value(params).expect("resolved params are valid Rune values");
    Some(json::to_value(&rune_val).expect("resolved params serialize to JSON"))
}

#[derive(Debug)]
pub enum RegisterFileError {
    Io(String, std::io::Error),
    Parse(String),
    Extension(String),
}

impl fmt::Display for RegisterFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterFileError::Io(path, e) => write!(f, "{path}: {e}"),
            RegisterFileError::Parse(rendered) => write!(f, "{rendered}"),
            RegisterFileError::Extension(path) => {
                write!(f, "{path}: scanner file must have a .rn extension")
            }
        }
    }
}

fn display_path(abs: &Path, manifest_name: &str) -> String {
    let home = std::env::var_os("HOME").map(|h| h.to_string_lossy().to_string());
    display_path_impl(abs, manifest_name, home.as_deref())
}

/// Elide a canonical bundle path (`…/{name}/scanner.rn`) to its prefix
/// and substitute `~` for the home directory. Elision is skipped when
/// the prefix is empty or itself ends in `.rn`, so the result stays
/// invertible: a path spec ending in `.rn` is always the literal file.
fn display_path_impl(abs: &Path, manifest_name: &str, home: Option<&str>) -> String {
    let s = abs.to_string_lossy().to_string();

    let bundle_suffix = format!("/{manifest_name}/scanner.rn");
    let s = match s.strip_suffix(&bundle_suffix) {
        Some(prefix) if !prefix.is_empty() && !prefix.ends_with(".rn") => prefix.to_string(),
        _ => s,
    };

    if let Some(home) = home {
        let home = home.trim_end_matches('/');
        if !home.is_empty() {
            if s == home {
                return "~".to_string();
            }
            if let Some(rest) = s.strip_prefix(home)
                && rest.starts_with('/')
            {
                return format!("~{rest}");
            }
        }
    }
    s
}

pub struct ScannerRegistry {
    defs: HashMap<String, ScannerDef>,
    errors: HashMap<String, String>,
}

impl ScannerRegistry {
    pub fn load() -> Self {
        extract_scanners().unwrap();
        let dir = scanners_dir();
        let mut defs = HashMap::new();
        let mut errors = HashMap::new();

        for path in walk_rn_files(&dir) {
            let rel = path
                .strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let code = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("{rel}: {e}");
                    continue;
                }
            };

            match parse_scanner(&code, &rel) {
                Ok(def) => {
                    let name = def.name.clone();
                    defs.insert(name, def);
                }
                Err(ParseError::MissingScanner) => {}
                Err(ref e) => {
                    let fallback_name = rel.split('/').next().unwrap_or(&rel).to_string();
                    errors.insert(fallback_name, e.render(&rel, &code));
                }
            }
        }

        ScannerRegistry { defs, errors }
    }

    /// Load a scanner from a filesystem path and register it under a
    /// composite name `{declared_name}[{display_path}]`. Returns the
    /// composite name on success — pass it to `Scanner::from_spec`.
    pub fn register_file(&mut self, path: &Path) -> Result<String, RegisterFileError> {
        if path.extension().is_none_or(|ext| ext != "rn") {
            return Err(RegisterFileError::Extension(path.display().to_string()));
        }
        let abs = path
            .canonicalize()
            .map_err(|e| RegisterFileError::Io(path.display().to_string(), e))?;
        let abs_str = abs.to_string_lossy().to_string();
        let code =
            std::fs::read_to_string(&abs).map_err(|e| RegisterFileError::Io(abs_str.clone(), e))?;

        let mut def = match parse_scanner(&code, &abs_str) {
            Ok(def) => def,
            Err(e) => return Err(RegisterFileError::Parse(e.render(&abs_str, &code))),
        };

        let display = display_path(&abs, &def.name);
        let composite = format!("{}[{}]", def.name, display);
        def.name = composite.clone();
        def.from_file = true;
        self.defs.insert(composite.clone(), def);
        Ok(composite)
    }

    pub fn get_def(&self, name: &str) -> Option<&ScannerDef> {
        self.defs.get(name)
    }

    pub fn parse_error(&self, name: &str) -> Option<&str> {
        self.errors.get(name).map(|s| s.as_str())
    }

    /// True if this name maps to a known scanner — successful parse
    /// or otherwise. Callers use this to distinguish "no such scanner"
    /// from "this scanner exists but failed to compile".
    pub fn is_known(&self, name: &str) -> bool {
        self.defs.contains_key(name) || self.errors.contains_key(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.defs.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn list(&self) -> Vec<&ScannerDef> {
        let mut defs: Vec<_> = self.defs.values().collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    pub fn list_visible(&self) -> Vec<&ScannerDef> {
        let mut defs: Vec<_> = self
            .defs
            .values()
            .filter(|d| !d.hidden && !d.from_file)
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Visible scanners that are enabled per the given config. Use
    /// this when picking the default set of scanners to run; use
    /// `list_visible` when listing all scanners regardless of enabled
    /// state.
    pub fn list_enabled(&self, config: &gage_core::config::Config) -> Vec<&ScannerDef> {
        self.list_visible()
            .into_iter()
            .filter(|d| config.is_scanner_enabled(&d.name))
            .collect()
    }

    /// Docstring for `note_name` from any scanner's `notes.writes`.
    /// Resolution order is by scanner name then task name (both ascending);
    /// last write wins for deterministic behaviour on duplicates. Returns
    /// `None` if no scanner declares the note.
    pub fn note_doc(&self, note_name: &str) -> Option<String> {
        let mut found: Option<String> = None;
        for def in self.list() {
            for task in def.tasks.values() {
                if let Some(doc) = task.notes.writes.get(note_name) {
                    found = Some(doc.clone());
                }
            }
        }
        found
    }
}

fn parse_scanner(source: &str, embed_key: &str) -> Result<ScannerDef, ParseError> {
    let file = parse::parse_all::<ast::File>(source, SourceId::empty(), false)
        .map_err(ParseError::Syntax)?;

    let mut scanner_name = None;
    let mut description = None;
    let mut version = String::new();
    let mut hidden = false;
    let mut tasks_obj: Option<&ast::ExprObject> = None;

    for (item, _) in &file.items {
        let ast::Item::Const(item_const) = item else {
            continue;
        };

        let ident_span = item_const.name.span();
        let ident = &source[ident_span.start.0 as usize..ident_span.end.0 as usize];

        if ident == "SCANNER" {
            let ast::Expr::Object(obj) = &item_const.expr else {
                continue;
            };
            for (field, _) in &obj.assignments {
                let key = field_key(source, &field.key);
                let Some((_, expr)) = &field.assign else {
                    continue;
                };
                match key.as_deref() {
                    Some("name") => scanner_name = expr_str(source, expr),
                    Some("description") => {
                        description = expr_str(source, expr).and_then(|s| {
                            s.lines()
                                .find(|l| !l.trim().is_empty())
                                .map(|l| l.trim().to_string())
                        })
                    }
                    Some("version") => {
                        version = expr_str(source, expr).unwrap_or_default();
                    }
                    Some("hidden") => {
                        if let ast::Expr::Lit(lit) = expr
                            && let ast::Lit::Bool(_) = &lit.lit
                        {
                            let span = expr.span();
                            let text = &source[span.start.0 as usize..span.end.0 as usize];
                            hidden = text == "true";
                        }
                    }
                    Some("tasks") => {
                        if let ast::Expr::Object(obj) = expr {
                            tasks_obj = Some(obj);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if scanner_name.is_none() {
        return Err(ParseError::MissingScanner);
    }

    let tasks = match tasks_obj {
        Some(obj) => parse_tasks(source, obj, embed_key)?,
        None => BTreeMap::new(),
    };

    Ok(ScannerDef {
        name: scanner_name.ok_or(ParseError::MissingName)?,
        description: description.unwrap_or_default(),
        version,
        hidden,
        tasks,
        ast: file,
        source: source.to_string(),
        embed_key: embed_key.to_string(),
        from_file: false,
    })
}

fn parse_tasks(
    source: &str,
    obj: &ast::ExprObject,
    embed_key: &str,
) -> Result<BTreeMap<String, TaskDef>, ParseError> {
    let mut tasks: BTreeMap<String, TaskDef> = BTreeMap::new();

    for (field, _) in &obj.assignments {
        let Some(name) = field_key(source, &field.key) else {
            continue;
        };
        let Some((_, expr)) = &field.assign else {
            continue;
        };
        let ast::Expr::Object(task_obj) = expr else {
            return Err(ParseError::TaskFieldType {
                task: name,
                field: "body",
            });
        };

        if tasks.contains_key(&name) {
            return Err(ParseError::DuplicateTask(name));
        }

        let mut notes = TaskNotesDef::default();

        for (tf, _) in &task_obj.assignments {
            let Some(tkey) = field_key(source, &tf.key) else {
                continue;
            };
            let Some((_, texpr)) = &tf.assign else {
                continue;
            };
            if tkey.as_str() == "notes" {
                let ast::Expr::Object(notes_obj) = texpr else {
                    return Err(ParseError::TaskFieldType {
                        task: name.clone(),
                        field: "notes",
                    });
                };
                notes = parse_task_notes(source, notes_obj, &name, embed_key)?;
            }
        }

        tasks.insert(name.clone(), TaskDef { name, notes });
    }

    Ok(tasks)
}

fn parse_task_notes(
    source: &str,
    obj: &ast::ExprObject,
    task: &str,
    embed_key: &str,
) -> Result<TaskNotesDef, ParseError> {
    let mut notes = TaskNotesDef::default();
    for (field, _) in &obj.assignments {
        let Some(key) = field_key(source, &field.key) else {
            continue;
        };
        let Some((_, expr)) = &field.assign else {
            continue;
        };
        match key.as_str() {
            "wants" => {
                let ast::Expr::Vec(vec_expr) = expr else {
                    return Err(ParseError::TaskFieldType {
                        task: task.to_string(),
                        field: "notes.wants",
                    });
                };
                for (item_expr, _) in &vec_expr.items {
                    let s = expr_str(source, item_expr).ok_or(ParseError::TaskFieldType {
                        task: task.to_string(),
                        field: "notes.wants",
                    })?;
                    notes.wants.push(s);
                }
            }
            "writes" => {
                let ast::Expr::Object(writes_obj) = expr else {
                    return Err(ParseError::TaskFieldType {
                        task: task.to_string(),
                        field: "notes.writes",
                    });
                };
                notes.writes = parse_notes(source, writes_obj, task, embed_key)?;
            }
            _ => {}
        }
    }
    Ok(notes)
}

fn parse_notes(
    source: &str,
    obj: &ast::ExprObject,
    task: &str,
    embed_key: &str,
) -> Result<BTreeMap<String, String>, ParseError> {
    let mut notes = BTreeMap::new();
    for (field, _) in &obj.assignments {
        let Some(key) = field_key(source, &field.key) else {
            continue;
        };
        let Some((_, expr)) = &field.assign else {
            continue;
        };
        let doc = resolve_note_doc(source, expr, task, &key, embed_key)?;
        notes.insert(key, doc);
    }
    Ok(notes)
}

/// Resolve a `notes.writes` docstring value. Accepts a string literal or
/// an `include_str!("relative/path")` macro call resolved against the
/// scanner's directory (same convention as the Rune `include_str!` macro
/// in [`runtime::macros`]).
fn resolve_note_doc(
    source: &str,
    expr: &ast::Expr,
    task: &str,
    note: &str,
    embed_key: &str,
) -> Result<String, ParseError> {
    if let Some(s) = expr_str(source, expr) {
        return Ok(s);
    }
    if let ast::Expr::MacroCall(mc) = expr {
        let path_span = mc.path.span();
        let macro_name = &source[path_span.start.0 as usize..path_span.end.0 as usize];
        if macro_name == "include_str" {
            let raw = {
                let open_end = mc.open.span.end.0 as usize;
                let close_start = mc.close.span.start.0 as usize;
                source[open_end..close_start].trim()
            };
            let stripped = strip_quotes(raw);
            if stripped.len() != raw.len() {
                let path = include_base_dir(embed_key).join(&stripped);
                return std::fs::read_to_string(&path)
                    .map(|s| s.trim_end().to_string())
                    .map_err(|e| ParseError::IncludeStr {
                        task: task.to_string(),
                        note: note.to_string(),
                        path,
                        message: e.to_string(),
                    });
            }
        }
    }
    Err(ParseError::TaskFieldType {
        task: task.to_string(),
        field: "notes.writes",
    })
}

/// Directory `include_str!` paths in `notes.writes` resolve against.
/// Mirrors `ScannerDef::module_dir`: absolute `embed_key` (a `--file`
/// scanner) → its parent; relative `embed_key` (embedded scanner) →
/// `scanners_dir()` + the leading subdirectory.
fn include_base_dir(embed_key: &str) -> PathBuf {
    let p = Path::new(embed_key);
    if p.is_absolute() {
        return p
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
    }
    let rel = embed_key.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    scanners_dir().join(rel)
}

fn parse_config(source: &str, obj: &ast::ExprObject) -> Object {
    let mut config = Object::new();

    for (field, _) in &obj.assignments {
        let Some(key) = field_key(source, &field.key) else {
            continue;
        };
        let Some((_, expr)) = &field.assign else {
            continue;
        };
        let ast::Expr::Object(entry_obj) = expr else {
            continue;
        };

        let mut entry = Object::new();
        for (entry_field, _) in &entry_obj.assignments {
            let Some(entry_key) = field_key(source, &entry_field.key) else {
                continue;
            };
            let Some((_, entry_expr)) = &entry_field.assign else {
                continue;
            };
            if let Some(val) = expr_to_value(source, entry_expr) {
                entry
                    .insert(alloc::String::try_from(entry_key.as_str()).unwrap(), val)
                    .unwrap();
            }
        }

        config
            .insert(
                alloc::String::try_from(key.as_str()).unwrap(),
                rune::to_value(entry).unwrap(),
            )
            .unwrap();
    }

    config
}

fn expr_to_value(source: &str, expr: &ast::Expr) -> Option<Value> {
    let ast::Expr::Lit(lit) = expr else {
        return expr_str(source, expr).map(|s| rune::to_value(s).unwrap());
    };
    match &lit.lit {
        ast::Lit::Number(_) => {
            let span = expr.span();
            let text = &source[span.start.0 as usize..span.end.0 as usize];
            if let Ok(i) = text.parse::<i64>() {
                Some(rune::to_value(i).unwrap())
            } else if let Ok(f) = text.parse::<f64>() {
                Some(rune::to_value(f).unwrap())
            } else {
                None
            }
        }
        ast::Lit::Str(_) => expr_str(source, expr).map(|s| rune::to_value(s).unwrap()),
        ast::Lit::Bool(b) => {
            let span = b.span();
            let text = &source[span.start.0 as usize..span.end.0 as usize];
            Some(rune::to_value(text == "true").unwrap())
        }
        _ => None,
    }
}

fn field_key(source: &str, key: &ast::ObjectKey) -> Option<String> {
    match key {
        ast::ObjectKey::Path(path) => {
            let span = path.span();
            Some(source[span.start.0 as usize..span.end.0 as usize].to_string())
        }
        ast::ObjectKey::LitStr(lit) => {
            let span = lit.span();
            Some(strip_quotes(
                &source[span.start.0 as usize..span.end.0 as usize],
            ))
        }
        _ => None,
    }
}

fn expr_str(source: &str, expr: &ast::Expr) -> Option<String> {
    let span = expr.span();
    let raw = &source[span.start.0 as usize..span.end.0 as usize];
    let stripped = strip_quotes(raw);
    if stripped.len() == raw.len() {
        return None;
    }
    Some(stripped.trim().to_string())
}

fn strip_quotes(s: &str) -> String {
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('`') && s.ends_with('`')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn walk_rn_files(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    walk_rn_files_rec(dir, &mut result);
    result.sort();
    result
}

fn walk_rn_files_rec(dir: &Path, result: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rn_files_rec(&path, result);
        } else if path.extension().is_some_and(|ext| ext == "rn") {
            result.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(path: &str, name: &str, home: Option<&str>) -> String {
        display_path_impl(Path::new(path), name, home)
    }

    #[test]
    fn display_path_elides_bundle_layout() {
        let home = Some("/home/u");
        assert_eq!(
            display("/home/u/scanners/foo/scanner.rn", "foo", home),
            "~/scanners"
        );
    }

    #[test]
    fn display_path_keeps_non_canonical_layouts_verbatim() {
        let home = Some("/home/u");
        assert_eq!(
            display("/home/u/scanners/bar/scanner.rn", "foo", home),
            "~/scanners/bar/scanner.rn"
        );
        assert_eq!(
            display("/home/u/scanners/scanner.rn", "foo", home),
            "~/scanners/scanner.rn"
        );
        assert_eq!(
            display("/home/u/scanners/foo.rn", "foo", home),
            "~/scanners/foo.rn"
        );
    }

    #[test]
    fn display_path_skips_elision_when_prefix_ends_in_rn() {
        assert_eq!(
            display("/x/weird.rn/foo/scanner.rn", "foo", None),
            "/x/weird.rn/foo/scanner.rn"
        );
    }

    #[test]
    fn display_path_skips_elision_when_prefix_is_empty() {
        assert_eq!(display("/foo/scanner.rn", "foo", None), "/foo/scanner.rn");
    }

    #[test]
    fn display_path_elides_bundle_directly_under_home() {
        assert_eq!(
            display("/home/u/foo/scanner.rn", "foo", Some("/home/u")),
            "~"
        );
    }

    #[test]
    fn display_path_home_requires_component_boundary() {
        assert_eq!(
            display("/home/ux/foo.rn", "foo", Some("/home/u")),
            "/home/ux/foo.rn"
        );
    }

    #[test]
    fn display_path_home_with_trailing_slash() {
        assert_eq!(
            display("/home/u/foo.rn", "foo", Some("/home/u/")),
            "~/foo.rn"
        );
    }
}
