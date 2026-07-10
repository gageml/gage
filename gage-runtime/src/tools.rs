//! `gage::tools` — builder types configuring the built-in Gage MCP
//! tools an agent receives via `call_agent(...).gage_tools([...])`.
//!
//! Each type mirrors one gage-mcp tool. `new()` gives defaults; `mut
//! self` setters configure. Bare string names in `gage_tools` dispatch
//! to the corresponding builder's defaults.

use gage_mcp::{GageTool, IssueWriteConfig, NoteWriteConfig, QueryConfig};
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

/// Write notes. `scan(id)` links writes to that scan and serves as the
/// fallback note target.
#[derive(Any, Debug, Clone, Default)]
#[rune(item = ::gage::tools)]
pub struct NoteWrite {
    scan: Option<String>,
}

impl NoteWrite {
    #[rune::function(path = Self::new)]
    fn new() -> Self {
        NoteWrite::default()
    }

    #[rune::function(instance)]
    fn scan(mut self, id: String) -> Self {
        self.scan = Some(id);
        self
    }
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

impl From<NoteWrite> for GageTool {
    fn from(t: NoteWrite) -> Self {
        GageTool::NoteWrite(NoteWriteConfig { scan: t.scan })
    }
}
