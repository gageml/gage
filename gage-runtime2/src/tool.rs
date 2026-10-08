//! Tools for `call_agent`: `Tool`, `Input`, and the Gage tool
//! configurations `Query` and `IssueWrite`.
//!
//! A `Tool` is a value with chained setters and no finalizer, of one of
//! two kinds. A scanner-defined tool, `Tool::new(name, handler)`, takes
//! the required data, and description, inputs, annotations, and
//! `requires_meta` chain. A Gage tool is one of the tools this runtime
//! provides, declared by its typed configuration value such as `Query`,
//! which `.tool()` accepts directly. `call_agent(..).tool(t)` consumes the tools when the agent
//! starts: names and inputs are validated, each scanner-defined tool
//! becomes a [`CustomToolDef`] whose callback runs the handler, and
//! each Gage tool becomes a [`GageTool`] carrying the data it serves.
//! `Query` carries a query context; `IssueWrite` carries a callback
//! that writes the issue through the runtime's own write path, under
//! the calling task's scan context and output sink as a scanner-defined
//! handler runs.
//!
//! The handler is held as a [`SyncFunction`], the `Send + Sync` form of
//! a Rune function, because the MCP service calls it from the host's
//! server tasks. The conversion fails for a closure that captures a
//! value with no constant form; that failure is reported when the
//! tools are consumed, not at `Tool::new`.

use std::collections::HashSet;
use std::sync::Arc;

use gage_core::uuid::short_uuid;
use gage_mcp2::{
    CustomToolCallback, CustomToolDef, CustomToolOutcome, GageTool, IssueWriteCallback,
    IssueWriteConfig, IssueWriteInput, QueryConfig, ToolAnnotations, ToolSpec,
};
use gage_runtime::dispatcher::ToolMeta;
use gage_runtime::error::Error;
use gage_runtime::value::{json_to_value, value_to_json};
use gage_store::IssueStatus;
use rmcp_json::JsonObject;
use rune::alloc::clone::TryClone;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{
    Formatter, FromValue, Function, Ref, RuntimeError, SyncFunction, ToValue, Value, VmError,
};
use rune::{Any, ContextError, Module};
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::{Instrument, Span};

use crate::issue::write_issue_tool;
use crate::scan::{SCAN_CTX, ScanContext, Session, render_vm_error, session_id};
use crate::{Level, OUTPUT_SINK, Output, OutputSink};

/// `rmcp::model::JsonObject` as gage-mcp2 exposes it through
/// `CustomToolDef::input_schema`
mod rmcp_json {
    pub type JsonObject = serde_json::Map<String, serde_json::Value>;
}

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.ty::<Tool>()?;
    m.function_meta(Tool::new)?;
    m.function_meta(Tool::description)?;
    m.function_meta(Tool::input)?;
    m.function_meta(Tool::inputs)?;
    m.function_meta(Tool::requires_meta)?;
    m.function_meta(Tool::read_only)?;
    m.function_meta(Tool::idempotent)?;
    m.function_meta(Tool::additive)?;
    m.function_meta(Tool::closed_world)?;
    m.function_meta(Tool::debug)?;
    m.ty::<Input>()?;
    m.function_meta(Input::string)?;
    m.function_meta(Input::integer)?;
    m.function_meta(Input::number)?;
    m.function_meta(Input::boolean)?;
    m.function_meta(Input::from_schema)?;
    m.function_meta(Input::required)?;
    m.function_meta(Input::description)?;
    m.function_meta(Input::debug)?;
    gage_runtime::dispatcher::register(&mut m)?;
    Ok(m)
}

/// `gage::tools`: the configuration values of the Gage tools, in the
/// module the legacy runtime kept them in
pub(crate) fn tools_module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate_item("gage", ["tools"])?;
    m.ty::<Query>()?;
    m.function_meta(Query::new)?;
    m.function_meta(Query::with_session)?;
    m.function_meta(Query::with_session_range)?;
    m.function_meta(Query::debug)?;
    m.ty::<IssueWrite>()?;
    m.function_meta(IssueWrite::new)?;
    m.function_meta(IssueWrite::name)?;
    m.function_meta(IssueWrite::pending)?;
    m.function_meta(IssueWrite::debug)?;
    Ok(m)
}

/// One tool of either kind
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Tool {
    #[rune(skip)]
    kind: ToolKind,
}

#[derive(Clone)]
enum ToolKind {
    Scanner(ScannerTool),
    Gage(GageConfig),
}

/// The configuration value of one Gage tool
#[derive(Clone, Debug)]
enum GageConfig {
    Query(Query),
    IssueWrite(IssueWrite),
}

impl GageConfig {
    /// The tool's wire name
    fn name(&self) -> &'static str {
        match self {
            GageConfig::Query(_) => gage_mcp2::tools::query::NAME,
            GageConfig::IssueWrite(_) => gage_mcp2::tools::issue_write::NAME,
        }
    }
}

impl From<Query> for Tool {
    fn from(query: Query) -> Self {
        Tool {
            kind: ToolKind::Gage(GageConfig::Query(query)),
        }
    }
}

impl From<IssueWrite> for Tool {
    fn from(config: IssueWrite) -> Self {
        Tool {
            kind: ToolKind::Gage(GageConfig::IssueWrite(config)),
        }
    }
}

/// The tool a `.tool()` / `.tools()` argument names: a `Tool`, or a
/// Gage tool's configuration value, which stands for the tool
pub(crate) fn tool_from_value(v: &Value) -> Result<Tool, VmError> {
    if let Ok(tool) = v.borrow_ref::<Tool>() {
        return Ok(tool.clone());
    }
    if let Ok(query) = v.borrow_ref::<Query>() {
        return Ok(Tool::from(query.clone()));
    }
    if let Ok(config) = v.borrow_ref::<IssueWrite>() {
        return Ok(Tool::from(config.clone()));
    }
    Err(VmError::panic(format!(
        "expected a Tool or a Gage tool such as Query or IssueWrite, got {}",
        v.type_info()
    )))
}

/// A scanner-defined tool
#[derive(Clone)]
struct ScannerTool {
    name: String,
    description: Option<String>,
    inputs: Vec<Input>,
    requires_meta: bool,
    /// `None` until an annotation method is called, so a tool with no
    /// annotations sends none
    annotations: Option<ToolAnnotations>,
    handler: Handler,
}

#[derive(Clone)]
enum Handler {
    Sync(Arc<SyncFunction>),
    /// The handler could not be made `Send`; the message says why
    Unsendable(String),
}

impl Tool {
    #[rune::function(path = Self::new)]
    fn new(name: Ref<str>, handler: Ref<Function>) -> Tool {
        let handler = match handler.try_clone().unwrap().into_sync() {
            Ok(f) => Handler::Sync(Arc::new(f)),
            Err(e) => Handler::Unsendable(e.to_string()),
        };
        Tool {
            kind: ToolKind::Scanner(ScannerTool {
                name: name.to_owned(),
                description: None,
                inputs: Vec::new(),
                requires_meta: false,
                annotations: None,
                handler,
            }),
        }
    }

    #[rune::function(instance)]
    fn description(mut self, text: Ref<str>) -> Self {
        if let Some(t) = self.scanner_mut("description") {
            t.description = Some(text.to_owned());
        }
        self
    }

    #[rune::function(instance)]
    fn input(mut self, input: Ref<Input>) -> Self {
        if let Some(t) = self.scanner_mut("input") {
            t.inputs.push(input.clone());
        }
        self
    }

    #[rune::function(instance)]
    fn inputs(mut self, list: Value) -> Result<Self, VmError> {
        let list = list.borrow_ref::<rune::runtime::Vec>()?;
        let mut inputs = Vec::with_capacity(list.len());
        for item in list.iter() {
            inputs.push(item.borrow_ref::<Input>()?.clone());
        }
        if let Some(t) = self.scanner_mut("inputs") {
            t.inputs.extend(inputs);
        }
        Ok(self)
    }

    /// The handler takes the request's meta as its second argument
    #[rune::function(instance)]
    fn requires_meta(mut self) -> Self {
        if let Some(t) = self.scanner_mut("requires_meta") {
            t.requires_meta = true;
        }
        self
    }

    /// The tool does not modify its environment
    #[rune::function(instance)]
    fn read_only(mut self) -> Self {
        if let Some(a) = self.annotations_mut("read_only") {
            a.read_only_hint = Some(true);
        }
        self
    }

    /// Repeated calls with the same inputs have no additional effect
    #[rune::function(instance)]
    fn idempotent(mut self) -> Self {
        if let Some(a) = self.annotations_mut("idempotent") {
            a.idempotent_hint = Some(true);
        }
        self
    }

    /// The tool performs only additive updates
    #[rune::function(instance)]
    fn additive(mut self) -> Self {
        if let Some(a) = self.annotations_mut("additive") {
            a.destructive_hint = Some(false);
        }
        self
    }

    /// The tool's domain of interaction is closed
    #[rune::function(instance)]
    fn closed_world(mut self) -> Self {
        if let Some(a) = self.annotations_mut("closed_world") {
            a.open_world_hint = Some(false);
        }
        self
    }

    /// The scanner tool behind a scanner-tool method. A Gage tool has
    /// none: its methods are unreachable from the value `.tool()`
    /// accepts, so the method name is unused here.
    fn scanner_mut(&mut self, _method: &'static str) -> Option<&mut ScannerTool> {
        match &mut self.kind {
            ToolKind::Scanner(t) => Some(t),
            ToolKind::Gage(_) => None,
        }
    }

    fn annotations_mut(&mut self, method: &'static str) -> Option<&mut ToolAnnotations> {
        self.scanner_mut(method)
            .map(|t| t.annotations.get_or_insert_with(ToolAnnotations::default))
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        match &self.kind {
            ToolKind::Scanner(t) => write!(
                f,
                "Tool {{ name: {:?}, description: {:?}, inputs: {}, requires_meta: {} }}",
                t.name,
                t.description,
                t.inputs.len(),
                t.requires_meta
            )?,
            ToolKind::Gage(config) => write!(f, "Tool {{ gage: {config:?} }}")?,
        }
        Ok(())
    }
}

/// The configuration of the Gage query tool: the scope its SQL runs
/// over
#[derive(Any, Clone, Debug)]
#[rune(item = ::gage::tools)]
pub struct Query {
    #[rune(skip)]
    scope: QueryScope,
}

#[derive(Clone, Debug)]
pub(crate) enum QueryScope {
    /// The scan's dataset
    Dataset,
    /// One member session, whole or a line range (inclusive)
    Session {
        id: String,
        lines: Option<(u64, u64)>,
    },
}

impl Query {
    /// The scan's dataset
    #[rune::function(path = Self::new)]
    fn new() -> Query {
        Query {
            scope: QueryScope::Dataset,
        }
    }

    /// One session
    #[rune::function(path = Self::with_session)]
    fn with_session(session: Ref<Session>) -> Query {
        Query {
            scope: QueryScope::Session {
                id: session.id.clone(),
                lines: None,
            },
        }
    }

    /// One session's lines `start` through `end`, inclusive, given
    /// as `session`, a `Session` or an id string, and `(start, end)`,
    /// the pair `sessions().unseen(key)` yields.
    #[rune::function(path = Self::with_session_range)]
    fn with_session_range(session: Value, range: (i64, i64)) -> Result<Query, VmError> {
        let (start, end) = range;
        if start < 1 || end < start {
            return Err(VmError::panic(format!(
                "Query::with_session_range: lines {start}..{end} is not a range from 1"
            )));
        }
        Ok(Query {
            scope: QueryScope::Session {
                id: session_id(&session)?,
                lines: Some((start as u64, end as u64)),
            },
        })
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "Query {{ scope: {:?} }}", self.scope)?;
        Ok(())
    }
}

/// The configuration of the Gage issue-writing tool: the name every
/// issue is written under and the initial status. The model supplies
/// the title, description, and evidence per call.
#[derive(Any, Clone, Debug)]
#[rune(item = ::gage::tools)]
pub struct IssueWrite {
    #[rune(skip)]
    name: String,
    #[rune(skip)]
    status: IssueStatus,
}

impl IssueWrite {
    /// Issues named `general`, written `open`
    #[rune::function(path = Self::new)]
    fn new() -> IssueWrite {
        IssueWrite {
            name: "general".into(),
            status: IssueStatus::Open,
        }
    }

    /// The name every issue is written under
    #[rune::function(instance)]
    fn name(mut self, name: Ref<str>) -> Self {
        self.name = name.to_owned();
        self
    }

    /// Write issues with status `pending`, for reconciliation by the
    /// resolve workflow, instead of `open`
    #[rune::function(instance)]
    fn pending(mut self) -> Self {
        self.status = IssueStatus::Pending;
        self
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "IssueWrite {{ name: {:?}, status: {:?} }}",
            self.name,
            self.status.as_str()
        )?;
        Ok(())
    }
}

/// One declared input of a tool
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Input {
    #[rune(skip)]
    name: String,
    #[rune(skip)]
    required: bool,
    #[rune(skip)]
    kind: InputKind,
    /// A subschema method applied to a schema-kind input, by name.
    /// The schema owns that keyword; reported when the tool is
    /// consumed.
    #[rune(skip)]
    conflict: Option<&'static str>,
}

#[derive(Clone)]
enum InputKind {
    /// A JSON Schema subschema, sent verbatim
    Schema(JsonValue),
    Simple {
        ty: SimpleType,
        description: Option<String>,
    },
}

#[derive(Clone, Copy)]
enum SimpleType {
    String,
    Integer,
    Number,
    Boolean,
}

impl SimpleType {
    fn schema_name(self) -> &'static str {
        match self {
            SimpleType::String => "string",
            SimpleType::Integer => "integer",
            SimpleType::Number => "number",
            SimpleType::Boolean => "boolean",
        }
    }
}

impl Input {
    #[rune::function(path = Self::string)]
    fn string(name: Ref<str>) -> Input {
        Input::simple(&name, SimpleType::String)
    }

    #[rune::function(path = Self::integer)]
    fn integer(name: Ref<str>) -> Input {
        Input::simple(&name, SimpleType::Integer)
    }

    /// A numeric input the handler receives as a float, whole or not
    #[rune::function(path = Self::number)]
    fn number(name: Ref<str>) -> Input {
        Input::simple(&name, SimpleType::Number)
    }

    #[rune::function(path = Self::boolean)]
    fn boolean(name: Ref<str>) -> Input {
        Input::simple(&name, SimpleType::Boolean)
    }

    fn simple(name: &str, ty: SimpleType) -> Input {
        Input {
            name: name.to_owned(),
            required: false,
            kind: InputKind::Simple {
                ty,
                description: None,
            },
            conflict: None,
        }
    }

    /// An input declared by a JSON Schema subschema, written as an
    /// object literal
    #[rune::function(path = Self::from_schema)]
    fn from_schema(name: Ref<str>, schema: Value) -> Result<Input, VmError> {
        let schema = value_to_json(&schema)
            .map_err(|e| VmError::panic(format!("input '{}': schema: {e}", &*name)))?;
        Ok(Input {
            name: name.to_owned(),
            required: false,
            kind: InputKind::Schema(schema),
            conflict: None,
        })
    }

    #[rune::function(instance)]
    fn required(mut self) -> Self {
        self.required = true;
        self
    }

    #[rune::function(instance)]
    fn description(mut self, text: Ref<str>) -> Self {
        match &mut self.kind {
            InputKind::Simple { description, .. } => *description = Some(text.to_owned()),
            InputKind::Schema(_) => self.conflict = Some("description"),
        }
        self
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        let kind = match &self.kind {
            InputKind::Schema(_) => "schema",
            InputKind::Simple { ty, .. } => ty.schema_name(),
        };
        write!(
            f,
            "Input {{ name: {:?}, kind: {kind}, required: {} }}",
            self.name, self.required
        )?;
        Ok(())
    }
}

/// Validate `tools` and build the service spec, with the tool names in
/// declaration order. Each scanner-defined handler runs under `ctx`
/// with no task params and, when the call is made from a task, under
/// that task's output sink, so the handler's output and writes are
/// attributed to the calling task. Each Gage tool's data is built
/// from `ctx`.
pub(crate) async fn consume(
    tools: &[Tool],
    ctx: &ScanContext,
    sink: Option<&OutputSink>,
) -> Result<(ToolSpec, Vec<String>), Error> {
    let mut spec = ToolSpec::default();
    let mut names: Vec<String> = Vec::with_capacity(tools.len());
    let mut seen = HashSet::new();
    for tool in tools {
        let name = match &tool.kind {
            ToolKind::Scanner(t) => {
                validate_name(&t.name)?;
                t.name.clone()
            }
            ToolKind::Gage(config) => config.name().to_string(),
        };
        if !seen.insert(name.clone()) {
            return Err(Error::agent(format!("tool '{name}' is declared twice")));
        }
        match &tool.kind {
            ToolKind::Scanner(t) => spec.custom.push(custom_def(t, ctx, sink)?),
            ToolKind::Gage(GageConfig::Query(query)) => {
                let context = ctx.query_tool_context(&query.scope).await?;
                spec.gage.push(GageTool::Query(QueryConfig {
                    context: Arc::new(context),
                }));
            }
            ToolKind::Gage(GageConfig::IssueWrite(config)) => {
                spec.gage.push(GageTool::IssueWrite(IssueWriteConfig {
                    callback: issue_write_callback(config, ctx, sink),
                }));
            }
        }
        names.push(name);
    }
    Ok((spec, names))
}

fn custom_def(
    tool: &ScannerTool,
    ctx: &ScanContext,
    sink: Option<&OutputSink>,
) -> Result<CustomToolDef, Error> {
    for input in &tool.inputs {
        if let Some(method) = input.conflict {
            return Err(Error::agent(format!(
                "tool '{}': input '{}' is declared by a schema, which owns \
                 `{method}`; set it in the schema",
                tool.name, input.name
            )));
        }
    }
    let handler = match &tool.handler {
        Handler::Sync(f) => Arc::clone(f),
        Handler::Unsendable(e) => {
            return Err(Error::agent(format!(
                "tool '{}': the handler captures a value that cannot be \
                 called from the MCP service: {e}",
                tool.name
            )));
        }
    };
    Ok(CustomToolDef {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: render_input_schema(&tool.inputs),
        annotations: tool.annotations.clone(),
        callback: callback(tool, handler, ctx, sink),
    })
}

/// The intersection of the MCP spec's tool-name rules and the Claude
/// API's `^[a-zA-Z0-9_-]{1,128}$`
fn validate_name(name: &str) -> Result<(), Error> {
    let valid = (1..=128).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(Error::agent(format!(
            "tool name {name:?} is not 1 to 128 ASCII letters, digits, '_', or '-'"
        )))
    }
}

/// The tool's input schema: one property per input, the required
/// list when any input is required, and no undeclared keys. A tool
/// with no inputs is the MCP spec's recommended empty-object form.
fn render_input_schema(inputs: &[Input]) -> JsonObject {
    let mut schema = JsonObject::new();
    schema.insert("type".into(), JsonValue::String("object".into()));
    if !inputs.is_empty() {
        let mut properties = JsonMap::new();
        let mut required = Vec::new();
        for input in inputs {
            let property = match &input.kind {
                InputKind::Schema(schema) => schema.clone(),
                InputKind::Simple { ty, description } => {
                    let mut property = JsonMap::new();
                    property.insert("type".into(), JsonValue::String(ty.schema_name().into()));
                    if let Some(d) = description {
                        property.insert("description".into(), JsonValue::String(d.clone()));
                    }
                    JsonValue::Object(property)
                }
            };
            properties.insert(input.name.clone(), property);
            if input.required {
                required.push(JsonValue::String(input.name.clone()));
            }
        }
        schema.insert("properties".into(), JsonValue::Object(properties));
        if !required.is_empty() {
            schema.insert("required".into(), JsonValue::Array(required));
        }
    }
    schema.insert("additionalProperties".into(), JsonValue::Bool(false));
    schema
}

/// The callback the MCP service runs for a call of `tool`: convert the
/// arguments, call the handler under the scan context and output
/// sink, and classify what it returned. Arguments that violate the
/// declared inputs are an error result the model reads; the handler
/// is not called. A handler that fails with a VM error is a fault;
/// the rendered error is logged to the task's sink. The service calls
/// the handler from the host's server tasks, outside the task's
/// task-locals, so each call runs under a `tool` span parented to
/// the span current where the callback is built, the calling agent's.
fn callback(
    tool: &ScannerTool,
    handler: Arc<SyncFunction>,
    ctx: &ScanContext,
    sink: Option<&OutputSink>,
) -> CustomToolCallback {
    let mut ctx = ctx.clone();
    ctx.params = None;
    let sink = sink.cloned();
    let declared = DeclaredInputs::new(&tool.inputs);
    let requires_meta = tool.requires_meta;
    let tool_name = tool.name.clone();
    let parent = Span::current();
    Arc::new(move |args, meta| {
        let span = tracing::info_span!(parent: &parent, "tool", name = %tool_name);
        let handler = Arc::clone(&handler);
        let ctx = ctx.clone();
        let sink = sink.clone();
        let scanner = ctx.scanner.clone();
        let log_sources = ctx.sources.clone();
        let tool_name = tool_name.clone();
        let args = declared.check(args);
        let call = async move {
            let args = match args {
                Ok(args) => args,
                Err(message) => return CustomToolOutcome::Error(message),
            };
            let inputs = InputsArg(args);
            let call = async move {
                if requires_meta {
                    let meta = ToolMeta::new(meta, scanner);
                    handler
                        .async_send_call::<HandlerOutcome>((inputs, meta))
                        .await
                } else {
                    handler.async_send_call::<HandlerOutcome>((inputs,)).await
                }
            };
            let scoped = SCAN_CTX.scope(ctx, call);
            let called = match &sink {
                Some(sink) => OUTPUT_SINK.scope(sink.clone(), scoped).await,
                None => scoped.await,
            };
            match called {
                Ok(outcome) => outcome.0,
                Err(e) => fault(&tool_name, &e, log_sources.as_deref(), sink.as_ref()),
            }
        };
        Box::pin(call.instrument(span))
    })
}

/// Report a tool that broke: the rendered VM error goes to the log and
/// to the task's sink, since the service calls the tool from the
/// host's server tasks, outside the scan's own record scope, and the
/// model receives a fault.
fn fault(
    tool_name: &str,
    e: &VmError,
    sources: Option<&rune::Sources>,
    sink: Option<&OutputSink>,
) -> CustomToolOutcome {
    let rendered = render_vm_error(e, sources);
    tracing::error!(error = %rendered, "tool handler failed");
    if let Some(sink) = sink {
        sink.send(Output::Log {
            level: Level::Error,
            message: format!("tool {tool_name} failed: {rendered}"),
        });
    }
    CustomToolOutcome::Fault("internal server error".into())
}

/// The callback the MCP service runs for an `IssueWrite` call: write
/// the issue under the calling task's scan context and output sink,
/// as a scanner-defined handler runs. The scanner's input error, such
/// as a cited note that does not exist, is an error result the model
/// reads; a VM error is a fault.
fn issue_write_callback(
    config: &IssueWrite,
    ctx: &ScanContext,
    sink: Option<&OutputSink>,
) -> IssueWriteCallback {
    let mut ctx = ctx.clone();
    ctx.params = None;
    let sink = sink.cloned();
    let name = config.name.clone();
    let status = config.status;
    let parent = Span::current();
    Arc::new(move |input: IssueWriteInput| {
        let tool_name = gage_mcp2::tools::issue_write::NAME;
        let span = tracing::info_span!(parent: &parent, "tool", name = %tool_name);
        let ctx = ctx.clone();
        let sink = sink.clone();
        let name = name.clone();
        let log_sources = ctx.sources.clone();
        let call = async move {
            let mut cited = input.evidence.clone();
            cited.sort();
            cited.dedup();
            let write =
                write_issue_tool(name, input.title, input.description, input.evidence, status);
            let scoped = SCAN_CTX.scope(ctx, write);
            let written = match &sink {
                Some(sink) => OUTPUT_SINK.scope(sink.clone(), scoped).await,
                None => scoped.await,
            };
            match written {
                Ok(Ok(issue)) => CustomToolOutcome::Success(JsonValue::String(format!(
                    "Wrote {} issue {} ({}) with {} evidence note(s).",
                    issue.status,
                    short_uuid(&issue.id),
                    issue.title,
                    cited.len()
                ))),
                Ok(Err(Error::Args(message))) => CustomToolOutcome::Error(message),
                Ok(Err(e)) => CustomToolOutcome::Error(e.to_string()),
                Err(e) => fault(tool_name, &e, log_sources.as_deref(), sink.as_ref()),
            }
        };
        Box::pin(call.instrument(span))
    })
}

/// The names a tool declares, for checking a call's arguments against
/// the schema the tool advertised
struct DeclaredInputs {
    names: Vec<String>,
    required: Vec<String>,
    floats: Vec<String>,
}

impl DeclaredInputs {
    fn new(inputs: &[Input]) -> Self {
        let is_float = |i: &Input| {
            matches!(
                i.kind,
                InputKind::Simple {
                    ty: SimpleType::Number,
                    ..
                }
            )
        };
        DeclaredInputs {
            names: inputs.iter().map(|i| i.name.clone()).collect(),
            required: inputs
                .iter()
                .filter(|i| i.required)
                .map(|i| i.name.clone())
                .collect(),
            floats: inputs
                .iter()
                .filter(|i| is_float(i))
                .map(|i| i.name.clone())
                .collect(),
        }
    }

    /// The arguments as the handler receives them, or the message for
    /// the model when a required input is missing or a key is not
    /// declared. A whole number sent for a `number` input becomes a
    /// float, so the handler sees the declared type.
    fn check(&self, args: JsonValue) -> Result<JsonValue, String> {
        let mut map = match args {
            JsonValue::Object(map) => map,
            _ => JsonMap::new(),
        };
        let missing: Vec<&str> = self
            .required
            .iter()
            .filter(|name| !map.contains_key(name.as_str()))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "missing required {}: {}",
                plural("input", missing.len()),
                missing.join(", ")
            ));
        }
        let undeclared: Vec<&str> = map
            .keys()
            .filter(|key| !self.names.contains(key))
            .map(String::as_str)
            .collect();
        if !undeclared.is_empty() {
            return Err(format!(
                "undeclared {}: {}",
                plural("input", undeclared.len()),
                undeclared.join(", ")
            ));
        }
        for name in &self.floats {
            if let Some(JsonValue::Number(n)) = map.get(name)
                && let Some(i) = n.as_i64()
            {
                map.insert(name.clone(), JsonValue::from(i as f64));
            }
        }
        Ok(JsonValue::Object(map))
    }
}

fn plural(noun: &str, count: usize) -> String {
    if count == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

/// The handler's first argument: the call's arguments as a Rune
/// object. A non-object becomes an empty object.
struct InputsArg(JsonValue);

impl ToValue for InputsArg {
    fn to_value(self) -> Result<Value, RuntimeError> {
        Ok(match self.0 {
            JsonValue::Object(_) => json_to_value(&self.0),
            _ => json_to_value(&JsonValue::Object(JsonMap::new())),
        })
    }
}

/// The handler's return value, classified. The handler contract is a
/// Rune `Result` whose arms hold a string or a JSON-encodable value;
/// anything else is a fault.
struct HandlerOutcome(CustomToolOutcome);

impl FromValue for HandlerOutcome {
    #[expect(
        clippy::disallowed_methods,
        reason = "takes the handler's return value; the call holds the only live handle"
    )]
    fn from_value(value: Value) -> Result<Self, RuntimeError> {
        let outcome = match rune::from_value::<Result<Value, Value>>(value) {
            Ok(Ok(v)) => match render(&v) {
                Ok(json) => CustomToolOutcome::Success(json),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "tool handler returned a value that cannot be encoded as JSON",
                    );
                    CustomToolOutcome::Fault("internal server error".into())
                }
            },
            Ok(Err(v)) => match render(&v) {
                Ok(JsonValue::String(s)) => CustomToolOutcome::Error(s),
                Ok(json) => CustomToolOutcome::Error(json.to_string()),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "tool handler returned an error that cannot be encoded as JSON",
                    );
                    CustomToolOutcome::Fault("internal server error".into())
                }
            },
            Err(_not_a_result) => {
                tracing::error!("tool handler returned a value that is not a Result");
                CustomToolOutcome::Fault("internal server error".into())
            }
        };
        Ok(HandlerOutcome(outcome))
    }
}

fn render(v: &Value) -> Result<JsonValue, String> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(JsonValue::String(s.to_string()));
    }
    value_to_json(v)
}

#[cfg(test)]
mod tests {
    use gage_session::{
        ContentSink, ContentSource, Driver, DriverError, NativeSession, Source, StoredSession,
    };
    use rune::runtime::Vm;
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source as RuneSource, Sources};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use gage_store::ScanDirLayout;

    /// A driver that runs nothing; tools need a context, not a harness
    struct NoDriver;

    impl Driver for NoDriver {
        fn name(&self) -> &'static str {
            "none"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &[]
        }
        fn open_source(&self, _source: &str) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            _sink: &mut dyn ContentSink,
        ) -> Result<String, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("not used".into()))
        }
    }

    fn scan_ctx(tmp: &TempDir) -> ScanContext {
        let store = tmp.path().join("store.git");
        gage_store::init(&store).unwrap();
        let paths = ScanDirLayout::new(tmp.path());
        let mut ctx =
            ScanContext::new("scan-1".into(), None, &store, paths, Arc::new(NoDriver)).unwrap();
        ctx.scanner = "demo".into();
        ctx
    }

    /// The tools `main()` returns in `script`
    fn tools(script: &str) -> Vec<Tool> {
        let mut vm = vm(script);
        let list = vm.call(["main"], ()).unwrap();
        let list = list.borrow_ref::<rune::runtime::Vec>().unwrap();
        list.iter()
            .map(|item| tool_from_value(item).unwrap())
            .collect()
    }

    fn vm(script: &str) -> Vm {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(RuneSource::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        Vm::new(rt, RuneArc::try_new(unit).unwrap())
    }

    async fn defs(tmp: &TempDir, script: &str) -> Result<Vec<CustomToolDef>, Error> {
        consume(&tools(script), &scan_ctx(tmp), None)
            .await
            .map(|(spec, _)| spec.custom)
    }

    async fn consume_err(tmp: &TempDir, script: &str) -> String {
        match defs(tmp, script).await {
            Ok(_) => panic!("expected consumption to fail"),
            Err(e) => e.to_string(),
        }
    }

    async fn call(def: &CustomToolDef, args: JsonValue, meta: JsonValue) -> CustomToolOutcome {
        (def.callback)(args, meta).await
    }

    #[tokio::test]
    async fn simple_inputs_render_as_properties_with_required_and_no_extra_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("lookup", |inputs| Ok("x"))
                    .description("Looks things up")
                    .input(Input::string("key").required().description("The key"))
                    .inputs([Input::integer("count"), Input::number("ratio"), Input::boolean("deep")])
                    .read_only()
                    .additive()]
            }
            "#,
        )
        .await
        .unwrap();
        let def = &defs[0];
        assert_eq!(def.name, "lookup");
        assert_eq!(def.description.as_deref(), Some("Looks things up"));
        assert_eq!(
            JsonValue::Object(def.input_schema.clone()),
            json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The key" },
                    "count": { "type": "integer" },
                    "ratio": { "type": "number" },
                    "deep": { "type": "boolean" },
                },
                "required": ["key"],
                "additionalProperties": false,
            })
        );
        let annotations = def.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, None);
        assert_eq!(annotations.open_world_hint, None);
    }

    #[tokio::test]
    async fn a_schema_input_is_sent_verbatim_and_required_still_applies() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("pick", |inputs| Ok("x"))
                    .input(Input::from_schema("unit", #{ type: "string", "enum": ["c", "f"] }).required())]
            }
            "#,
        )
        .await
        .unwrap();
        assert_eq!(
            JsonValue::Object(defs[0].input_schema.clone()),
            json!({
                "type": "object",
                "properties": { "unit": { "type": "string", "enum": ["c", "f"] } },
                "required": ["unit"],
                "additionalProperties": false,
            })
        );
    }

    #[tokio::test]
    async fn a_tool_without_inputs_description_or_annotations_sends_none_of_them() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::Tool;
            pub fn main() { [Tool::new("ping", |inputs| Ok("pong"))] }
            "#,
        )
        .await
        .unwrap();
        assert_eq!(defs[0].description, None);
        assert!(defs[0].annotations.is_none());
        assert_eq!(
            JsonValue::Object(defs[0].input_schema.clone()),
            json!({ "type": "object", "additionalProperties": false })
        );
    }

    #[tokio::test]
    async fn consumption_rejects_bad_names_duplicates_and_schema_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let err = async |script: &str| consume_err(&tmp, script).await;
        assert_eq!(
            err(r#"
            use gage::Tool;
            pub fn main() { [Tool::new("bad name", |i| Ok(1))] }
            "#)
            .await,
            "agent: tool name \"bad name\" is not 1 to 128 ASCII letters, digits, '_', or '-'"
        );
        assert_eq!(
            err(r#"
            use gage::Tool;
            pub fn main() { [Tool::new("a", |i| Ok(1)), Tool::new("a", |i| Ok(2))] }
            "#)
            .await,
            "agent: tool 'a' is declared twice"
        );
        assert_eq!(
            err(r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("a", |i| Ok(1))
                    .input(Input::from_schema("x", #{ type: "string" }).description("no"))]
            }
            "#)
            .await,
            "agent: tool 'a': input 'x' is declared by a schema, which owns `description`; set it in the schema"
        );
    }

    #[tokio::test]
    async fn a_handler_capturing_a_non_constant_value_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = consume_err(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                let captured = Input::string("x");
                [Tool::new("a", |i| Ok(captured))]
            }
            "#,
        )
        .await;
        assert!(
            err.starts_with("agent: tool 'a': the handler captures a value"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn ok_strings_pass_through_and_other_values_encode_as_json() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("text", |inputs| Ok("plain")),
                 Tool::new("data", |inputs| Ok(#{ key: inputs.key, n: [1, 2] }))
                    .input(Input::string("key"))]
            }
            "#,
        )
        .await
        .unwrap();
        assert_eq!(
            call(&defs[0], json!({}), json!({})).await,
            CustomToolOutcome::Success(json!("plain"))
        );
        assert_eq!(
            call(&defs[1], json!({"key": "k"}), json!({})).await,
            CustomToolOutcome::Success(json!({"key": "k", "n": [1, 2]}))
        );
    }

    #[tokio::test]
    async fn err_is_the_error_result_the_model_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::Tool;
            pub fn main() {
                [Tool::new("text", |inputs| Err("no such key")),
                 Tool::new("data", |inputs| Err(#{ code: 4 }))]
            }
            "#,
        )
        .await
        .unwrap();
        assert_eq!(
            call(&defs[0], json!({}), json!({})).await,
            CustomToolOutcome::Error("no such key".into())
        );
        assert_eq!(
            call(&defs[1], json!({}), json!({})).await,
            CustomToolOutcome::Error(r#"{"code":4}"#.into())
        );
    }

    #[tokio::test]
    async fn contract_violations_are_faults() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("bare", |inputs| "not a result"),
                 Tool::new("opaque", |inputs| Ok(Input::string("x"))),
                 Tool::new("broken", |inputs| Ok(inputs.missing))]
            }
            "#,
        )
        .await
        .unwrap();
        let fault = |o: CustomToolOutcome| match o {
            CustomToolOutcome::Fault(m) => m,
            other => panic!("expected a fault, got {other:?}"),
        };
        assert_eq!(
            fault(call(&defs[0], json!({}), json!({})).await),
            "internal server error"
        );
        assert_eq!(
            fault(call(&defs[1], json!({}), json!({})).await),
            "internal server error"
        );
        assert_eq!(
            fault(call(&defs[2], json!({}), json!({})).await),
            "internal server error"
        );
    }

    #[tokio::test]
    async fn number_inputs_arrive_as_floats_and_integers_as_ints() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("kinds", |inputs| Ok(#{
                    n: inputs.n is f64,
                    i: inputs.i is i64,
                    n_value: inputs.n,
                }))
                .inputs([Input::number("n"), Input::integer("i")])]
            }
            "#,
        )
        .await
        .unwrap();
        assert_eq!(
            call(&defs[0], json!({"n": 3, "i": 3}), json!({})).await,
            CustomToolOutcome::Success(json!({"n": true, "i": true, "n_value": 3.0}))
        );
    }

    #[tokio::test]
    async fn requires_meta_passes_the_request_meta_as_the_second_argument() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::Tool;
            pub fn main() {
                [Tool::new("who", |inputs, meta| Ok(meta.agent_tool_use())).requires_meta(),
                 Tool::new("plain", |inputs| Ok("one arg"))]
            }
            "#,
        )
        .await
        .unwrap();
        let meta = json!({"claudecode/toolUseId": "toolu_1"});
        assert_eq!(
            call(&defs[0], json!({}), meta.clone()).await,
            CustomToolOutcome::Success(json!("agent:demo?call=toolu_1"))
        );
        assert_eq!(
            call(&defs[1], json!({}), meta).await,
            CustomToolOutcome::Success(json!("one arg"))
        );
    }

    /// A handler runs under the calling task's output sink: its output
    /// reaches the scan's channel under the task's name, and a note it
    /// writes is attributed to the task.
    #[tokio::test]
    async fn handlers_run_under_the_calling_task_sink() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = scan_ctx(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "demo".into(),
            task: "review".into(),
            tx,
        };
        let (spec, _) = consume(
            &tools(
                r#"
                use gage::{Input, Tool, write_note};
                async fn record(inputs) {
                    println!("recording {}", inputs.text);
                    let note = write_note("finding", inputs.text).await?;
                    Ok(note.author)
                }
                pub fn main() { [Tool::new("record", record).input(Input::string("text"))] }
                "#,
            ),
            &ctx,
            Some(&sink),
        )
        .await
        .unwrap();
        let defs = spec.custom;
        assert_eq!(
            call(&defs[0], json!({"text": "hello"}), json!({})).await,
            CustomToolOutcome::Success(json!("task:demo:review"))
        );
        let out = rx.recv().await.unwrap();
        assert_eq!(
            (out.scanner.as_str(), out.task.as_str()),
            ("demo", "review")
        );
        assert_eq!(out.output, crate::Output::Println("recording hello".into()));
    }

    /// Arguments are checked against the declared inputs before the
    /// handler runs: a missing required input or an undeclared key is
    /// an error result naming the inputs, and the handler is not
    /// called. Null arguments count as an empty object.
    #[tokio::test]
    async fn arguments_are_checked_against_the_declared_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::{Input, Tool};
            pub fn main() {
                [Tool::new("finding", |inputs| Ok(inputs.description))
                    .input(Input::string("description").required())
                    .input(Input::string("session_id").required())
                    .input(Input::string("lines"))]
            }
            "#,
        )
        .await
        .unwrap();
        let finding = &defs[0];
        assert_eq!(
            call(
                finding,
                json!({"session_id": "s</parameter>junk"}),
                json!({})
            )
            .await,
            CustomToolOutcome::Error("missing required input: description".into())
        );
        assert_eq!(
            call(finding, JsonValue::Null, json!({})).await,
            CustomToolOutcome::Error("missing required inputs: description, session_id".into())
        );
        assert_eq!(
            call(
                finding,
                json!({"description": "d", "session_id": "s", "extra": 1, "more": 2}),
                json!({})
            )
            .await,
            CustomToolOutcome::Error("undeclared inputs: extra, more".into())
        );
        assert_eq!(
            call(
                finding,
                json!({"description": "d", "session_id": "s", "lines": "1-3"}),
                json!({})
            )
            .await,
            CustomToolOutcome::Success(json!("d"))
        );
        assert_eq!(
            call(
                finding,
                json!({"description": "d", "session_id": "s"}),
                json!({})
            )
            .await,
            CustomToolOutcome::Success(json!("d"))
        );
    }

    /// A handler that fails with a VM error logs the rendered error
    /// to the calling task's sink, so the fault reaches the scan
    /// record under the task's name.
    #[tokio::test]
    async fn handler_faults_log_to_the_calling_task_sink() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = scan_ctx(&tmp);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "demo".into(),
            task: "review".into(),
            tx,
        };
        let (spec, _) = consume(
            &tools(
                r#"
                use gage::Tool;
                pub fn main() { [Tool::new("broken", |inputs| Ok(inputs.missing))] }
                "#,
            ),
            &ctx,
            Some(&sink),
        )
        .await
        .unwrap();
        assert_eq!(
            call(&spec.custom[0], json!({}), json!({})).await,
            CustomToolOutcome::Fault("internal server error".into())
        );
        let out = rx.recv().await.unwrap();
        assert_eq!(
            (out.scanner.as_str(), out.task.as_str()),
            ("demo", "review")
        );
        let Output::Log { level, message } = out.output else {
            panic!("expected a log record, got {:?}", out.output);
        };
        assert_eq!(level, Level::Error);
        assert!(
            message.starts_with("tool broken failed: Missing index"),
            "{message}"
        );
    }

    /// `Query::with_session_range` takes the session as a `Session`
    /// or an id string and the range as the `(start, end)` pair
    /// `unseen` yields.
    #[test]
    fn query_with_session_range_takes_session_or_id_and_a_range_pair() {
        let scope = |v: &Value| match &tool_from_value(v).unwrap().kind {
            ToolKind::Gage(GageConfig::Query(q)) => q.scope.clone(),
            _ => panic!("expected a Query tool"),
        };
        let mut vm = vm(r#"
            use gage::tools::Query;
            pub fn main(s, range) { Query::with_session_range(s, range) }
        "#);
        let session = Session {
            id: "abc".into(),
            line_count: 9,
            commit: "c".into(),
        };
        let from_session = vm.call(["main"], (session, (1i64, 9i64))).unwrap();
        assert!(matches!(
            scope(&from_session),
            QueryScope::Session { ref id, lines: Some((1, 9)) } if id == "abc"
        ));
        let from_id = vm.call(["main"], ("abc", (2i64, 5i64))).unwrap();
        assert!(matches!(
            scope(&from_id),
            QueryScope::Session { ref id, lines: Some((2, 5)) } if id == "abc"
        ));
        let err = vm.call(["main"], ("abc", (3i64, 2i64))).unwrap_err();
        let rendered = render_vm_error(&err, None);
        assert!(
            rendered.contains("lines 3..2 is not a range from 1"),
            "{rendered}"
        );
    }

    /// A `Query` value consumes to the query tool over the scan's
    /// dataset, named `Query` alongside the scanner tools.
    #[tokio::test]
    async fn a_gage_query_tool_joins_the_spec_under_its_wire_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (spec, names) = consume(
            &tools(
                r#"
                use gage::{Tool, tools::Query};
                pub fn main() { [Tool::new("ping", |i| Ok("pong")), Query::new()] }
                "#,
            ),
            &scan_ctx(&tmp),
            None,
        )
        .await
        .unwrap();
        assert_eq!(names, ["ping", "Query"]);
        assert_eq!(spec.custom.len(), 1);
        assert_eq!(spec.gage.len(), 1);
        assert_eq!(spec.gage[0].name(), "Query");
    }

    #[tokio::test]
    async fn gage_tools_are_subject_to_name_uniqueness() {
        let tmp = tempfile::tempdir().unwrap();
        let err = async |script: &str| consume_err(&tmp, script).await;
        assert_eq!(
            err(r#"
            use gage::{Tool, tools::Query};
            pub fn main() { [Query::new(), Query::new()] }
            "#)
            .await,
            "agent: tool 'Query' is declared twice"
        );
        assert_eq!(
            err(r#"
            use gage::{Tool, tools::Query};
            pub fn main() { [Tool::new("Query", |i| Ok(1)), Query::new()] }
            "#)
            .await,
            "agent: tool 'Query' is declared twice"
        );
    }

    #[test]
    fn tool_values_accept_tools_and_gage_configs_only() {
        let err = match tool_from_value(&rune::to_value(7i64).unwrap()) {
            Ok(_) => panic!("an integer is not a tool"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("expected a Tool or a Gage tool"), "{err}");
    }

    /// The consumed `IssueWrite` tool: its config and the wire names
    /// the spec declares.
    async fn issue_write_tool(
        tmp: &TempDir,
        script: &str,
        sink: Option<&OutputSink>,
    ) -> (gage_mcp2::IssueWriteConfig, Vec<String>) {
        let (spec, names) = consume(&tools(script), &scan_ctx(tmp), sink).await.unwrap();
        let config = spec
            .gage
            .into_iter()
            .find_map(|t| match t {
                GageTool::IssueWrite(c) => Some(c),
                GageTool::Query(_) => None,
            })
            .expect("the spec holds the IssueWrite tool");
        (config, names)
    }

    #[tokio::test]
    async fn an_issue_write_tool_writes_an_issue_under_the_task_sink() {
        use gage_store::{IssueStatus, IssueStore};

        let tmp = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "demo".into(),
            task: "report".into(),
            tx,
        };
        let (config, names) = issue_write_tool(
            &tmp,
            r#"
            use gage::tools::{IssueWrite, Query};
            pub fn main() { [IssueWrite::new().name("findings").pending(), Query::new()] }
            "#,
            Some(&sink),
        )
        .await;
        assert_eq!(names, ["IssueWrite", "Query"]);
        let outcome = (config.callback)(IssueWriteInput {
            title: "Flaky build".into(),
            description: Some("## Summary\nIt flakes.".into()),
            evidence: Vec::new(),
        })
        .await;
        let CustomToolOutcome::Success(JsonValue::String(text)) = outcome else {
            panic!("unexpected outcome {outcome:?}");
        };
        assert!(text.starts_with("Wrote pending issue "), "{text}");
        assert!(
            text.ends_with("(Flaky build) with 0 evidence note(s)."),
            "{text}"
        );

        let ctx = scan_ctx(&tmp);
        let dirs: Vec<_> = std::fs::read_dir(ctx.paths.issues_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs.len(), 1);
        let store = ctx.store.lock().await;
        let record = IssueStore::from(&*store).read_from_dir(&dirs[0]).unwrap();
        assert_eq!(record.name, "findings");
        assert_eq!(record.title, "Flaky build");
        assert_eq!(
            record.description.as_deref(),
            Some("## Summary\nIt flakes.")
        );
        assert_eq!(record.status, IssueStatus::Pending);
        assert_eq!(record.author, "task:demo:report");
        assert_eq!(record.scan.as_deref(), Some("scan-1"));
        assert!(record.evidence.is_empty());
    }

    #[tokio::test]
    async fn an_issue_write_tool_reports_bad_evidence_to_the_model() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "demo".into(),
            task: "report".into(),
            tx,
        };
        let (config, _) = issue_write_tool(
            &tmp,
            r#"
            use gage::tools::IssueWrite;
            pub fn main() { [IssueWrite::new()] }
            "#,
            Some(&sink),
        )
        .await;
        let outcome = (config.callback)(IssueWriteInput {
            title: "t".into(),
            description: None,
            evidence: vec!["not-a-note".into()],
        })
        .await;
        let CustomToolOutcome::Error(message) = outcome else {
            panic!("unexpected outcome {outcome:?}");
        };
        assert!(message.starts_with("write_issue evidence: "), "{message}");
        assert!(
            !std::fs::exists(scan_ctx(&tmp).paths.issues_dir()).unwrap(),
            "nothing is written for a rejected write"
        );
    }

    #[tokio::test]
    async fn an_issue_write_tool_is_subject_to_name_uniqueness() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            consume_err(
                &tmp,
                r#"
                use gage::tools::IssueWrite;
                pub fn main() { [IssueWrite::new(), IssueWrite::new().name("x")] }
                "#
            )
            .await,
            "agent: tool 'IssueWrite' is declared twice"
        );
    }

    #[tokio::test]
    async fn async_handlers_are_awaited() {
        let tmp = tempfile::tempdir().unwrap();
        let defs = defs(
            &tmp,
            r#"
            use gage::Tool;
            async fn later(inputs) { Ok("awaited") }
            pub fn main() { [Tool::new("later", later)] }
            "#,
        )
        .await
        .unwrap();
        assert_eq!(
            call(&defs[0], json!({}), json!({})).await,
            CustomToolOutcome::Success(json!("awaited"))
        );
    }
}
