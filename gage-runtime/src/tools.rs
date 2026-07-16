//! `gage::tools` — builder types configuring the built-in Gage MCP
//! tools an agent receives via `call_agent(...).gage_tools([...])`.
//!
//! Each type mirrors one gage-mcp tool. `new()` gives defaults; `mut
//! self` setters configure. Bare string names in `gage_tools` dispatch
//! to the corresponding builder's defaults.

use std::collections::BTreeMap;

use gage_db::issue::IssueStatus;
use gage_mcp::{GageTool, IssueWriteConfig, NoteWriteConfig, QueryConfig};
use rune::runtime::Object;
use rune::{Any, ContextError, Module};

use crate::state::current_scan_ctx;

pub fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate_item("gage", ["tools"])?;
    m.ty::<Query>()?;
    m.function_meta(Query::new)?;
    m.function_meta(Query::scan)?;
    m.function_meta(Query::global)?;
    m.ty::<IssueWrite>()?;
    m.function_meta(IssueWrite::new)?;
    m.function_meta(IssueWrite::name)?;
    m.function_meta(IssueWrite::scan)?;
    m.function_meta(IssueWrite::global)?;
    m.function_meta(IssueWrite::status)?;
    m.ty::<NoteWrite>()?;
    m.function_meta(NoteWrite::new)?;
    m.function_meta(NoteWrite::names)?;
    m.function_meta(NoteWrite::scan)?;
    m.function_meta(NoteWrite::global)?;
    m.ty::<IssueClose>()?;
    m.function_meta(IssueClose::new)?;
    m.ty::<IssueComment>()?;
    m.function_meta(IssueComment::new)?;
    Ok(m)
}

/// Scan scoping shared by the tool builders. Defaults to the current
/// scan; `scan(id)` targets a specific scan; `global()` removes the
/// scoping entirely.
#[derive(Debug, Clone, Default)]
enum ScanScope {
    #[default]
    Current,
    Id(String),
    Global,
}

impl ScanScope {
    fn resolve(self) -> Option<String> {
        match self {
            ScanScope::Current => Some(current_scan_ctx().run.scan_id.clone()),
            ScanScope::Id(id) => Some(id),
            ScanScope::Global => None,
        }
    }
}

/// SQL query surface over Gage data. Scoped to the current scan by
/// default; `scan(id)` targets another scan; `global()` removes the
/// scoping so the context spans all scans.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct Query {
    scan: ScanScope,
}

impl Query {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        Query::default()
    }

    #[rune::function(instance)]
    fn scan(mut self, id: String) -> Self {
        self.scan = ScanScope::Id(id);
        self
    }

    #[rune::function(instance)]
    fn global(mut self) -> Self {
        self.scan = ScanScope::Global;
        self
    }
}

/// Write issues. `name(..)` sets the issue name for every write
/// (default `"general"`); writes link to the current scan by default —
/// `scan(id)` links to another scan, `global()` removes the link;
/// `status(..)` sets the initial status, `"open"` or `"pending"`
/// (default `"pending"`).
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct IssueWrite {
    name: Option<String>,
    scan: ScanScope,
    /// Parsed `status(..)` argument. The parse error is deferred so
    /// the builder chain stays fluent; `gage_tools` parsing surfaces
    /// it.
    status: Option<Result<IssueStatus, String>>,
}

impl IssueWrite {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        IssueWrite::default()
    }

    #[rune::function(instance)]
    fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    #[rune::function(instance)]
    fn scan(mut self, id: String) -> Self {
        self.scan = ScanScope::Id(id);
        self
    }

    #[rune::function(instance)]
    fn global(mut self) -> Self {
        self.scan = ScanScope::Global;
        self
    }

    #[rune::function(instance)]
    fn status(mut self, status: String) -> Self {
        self.status = Some(parse_status(&status));
        self
    }
}

fn parse_status(status: &str) -> Result<IssueStatus, String> {
    match status {
        "open" => Ok(IssueStatus::Open),
        "pending" => Ok(IssueStatus::Pending),
        other => Err(format!(
            "IssueWrite status must be \"open\" or \"pending\", got \"{other}\""
        )),
    }
}

/// Write notes. `names(#{ name: doc })` sets the allowed note names
/// with their docstrings (default `comment` only); writes link to the
/// current scan by default, which also serves as the fallback note
/// target — `scan(id)` links to another scan, `global()` removes the
/// link.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct NoteWrite {
    /// Parsed `names(..)` argument. The parse error is deferred so the
    /// builder chain stays fluent; `gage_tools` parsing surfaces it.
    names: Option<Result<BTreeMap<String, String>, String>>,
    scan: ScanScope,
}

impl NoteWrite {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        NoteWrite::default()
    }

    #[rune::function(instance)]
    fn names(mut self, names: Object) -> Self {
        self.names = Some(parse_names(&names));
        self
    }

    #[rune::function(instance)]
    fn scan(mut self, id: String) -> Self {
        self.scan = ScanScope::Id(id);
        self
    }

    #[rune::function(instance)]
    fn global(mut self) -> Self {
        self.scan = ScanScope::Global;
        self
    }
}

fn parse_names(names: &Object) -> Result<BTreeMap<String, String>, String> {
    if names.is_empty() {
        return Err("NoteWrite names must not be empty".to_string());
    }
    let mut out = BTreeMap::new();
    for (name, doc) in names.iter() {
        let doc: String = rune::from_value(doc.clone())
            .map_err(|e| format!("NoteWrite names['{name}'] must be a doc string: {e}"))?;
        out.insert(name.to_string(), doc);
    }
    Ok(out)
}

/// Close issues. No settings.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct IssueClose;

impl IssueClose {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        IssueClose
    }
}

/// Comment on issues. No settings.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct IssueComment;

impl IssueComment {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        IssueComment
    }
}

impl From<Query> for GageTool {
    fn from(t: Query) -> Self {
        GageTool::Query(QueryConfig {
            scan: t.scan.resolve(),
        })
    }
}

impl TryFrom<IssueWrite> for GageTool {
    type Error = String;

    fn try_from(t: IssueWrite) -> Result<Self, String> {
        let mut config = IssueWriteConfig::default();
        if let Some(name) = t.name {
            config.name = name;
        }
        config.scan = t.scan.resolve();
        if let Some(status) = t.status {
            config.status = status?;
        }
        Ok(GageTool::IssueWrite(config))
    }
}

impl TryFrom<NoteWrite> for GageTool {
    type Error = String;

    fn try_from(t: NoteWrite) -> Result<Self, String> {
        let mut config = NoteWriteConfig::default();
        if let Some(names) = t.names {
            config.names = names?;
        }
        config.scan = t.scan.resolve();
        Ok(GageTool::NoteWrite(config))
    }
}

/// Apply the default scan scoping to a tool built from a bare name
/// (or the `"*"` expansion): every tool that accepts a scan id gets
/// the current scan's.
pub(crate) fn apply_default_scan(tool: GageTool) -> GageTool {
    let scan_id = || Some(current_scan_ctx().run.scan_id.clone());
    match tool {
        GageTool::Query(mut c) => {
            c.scan = scan_id();
            GageTool::Query(c)
        }
        GageTool::IssueWrite(mut c) => {
            c.scan = scan_id();
            GageTool::IssueWrite(c)
        }
        GageTool::NoteWrite(mut c) => {
            c.scan = scan_id();
            GageTool::NoteWrite(c)
        }
        other => other,
    }
}
