//! `gage::tools` — builder types configuring the built-in Gage MCP
//! tools an agent receives via `call_agent(...).gage_tools([...])`.
//!
//! Each type mirrors one gage-mcp tool. `new()` gives defaults; `mut
//! self` setters configure. Bare string names in `gage_tools` dispatch
//! to the corresponding builder's defaults.

use std::collections::BTreeMap;

use gage_mcp::{GageTool, IssueWriteConfig, NoteWriteConfig, QueryConfig};
use rune::runtime::Object;
use rune::{Any, ContextError, Module};

pub fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate_item("gage", ["tools"])?;
    m.ty::<Query>()?;
    m.function_meta(Query::new)?;
    m.function_meta(Query::scan)?;
    m.ty::<IssueWrite>()?;
    m.function_meta(IssueWrite::new)?;
    m.function_meta(IssueWrite::name)?;
    m.function_meta(IssueWrite::scan)?;
    m.ty::<NoteWrite>()?;
    m.function_meta(NoteWrite::new)?;
    m.function_meta(NoteWrite::names)?;
    m.function_meta(NoteWrite::scan)?;
    m.ty::<IssueClose>()?;
    m.function_meta(IssueClose::new)?;
    m.ty::<IssueComment>()?;
    m.function_meta(IssueComment::new)?;
    Ok(m)
}

/// SQL query surface over Gage data. Unscoped by default; `scan(id)`
/// limits the context to rows linked to that scan.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct Query {
    scan: Option<String>,
}

impl Query {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        Query::default()
    }

    #[rune::function(instance)]
    fn scan(mut self, id: String) -> Self {
        self.scan = Some(id);
        self
    }
}

/// Write pending issues. `name(..)` sets the issue name for every
/// write (default `"general"`); `scan(id)` links writes to that scan.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct IssueWrite {
    name: Option<String>,
    scan: Option<String>,
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
        self.scan = Some(id);
        self
    }
}

/// Write notes. `names(#{ name: doc })` sets the allowed note names
/// with their docstrings (default `comment` only); `scan(id)` links
/// writes to that scan and serves as the fallback note target.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct NoteWrite {
    /// Parsed `names(..)` argument. The parse error is deferred so the
    /// builder chain stays fluent; `gage_tools` parsing surfaces it.
    names: Option<Result<BTreeMap<String, String>, String>>,
    scan: Option<String>,
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
        self.scan = Some(id);
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
        GageTool::Query(QueryConfig { scan: t.scan })
    }
}

impl From<IssueWrite> for GageTool {
    fn from(t: IssueWrite) -> Self {
        let mut config = IssueWriteConfig::default();
        if let Some(name) = t.name {
            config.name = name;
        }
        config.scan = t.scan;
        GageTool::IssueWrite(config)
    }
}

impl TryFrom<NoteWrite> for GageTool {
    type Error = String;

    fn try_from(t: NoteWrite) -> Result<Self, String> {
        let mut config = NoteWriteConfig::default();
        if let Some(names) = t.names {
            config.names = names?;
        }
        config.scan = t.scan;
        Ok(GageTool::NoteWrite(config))
    }
}
