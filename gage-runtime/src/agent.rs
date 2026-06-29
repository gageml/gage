//! `call_agent(prompt)` — Rune builder that, on await, spawns an
//! isolated `claude -p` child driven by an in-process MCP service
//! exposing a per-call tool set, and returns an [`Agent`] handle.
//!
//! This module owns the Rune-visible surface and the parsing of
//! builder arguments. The actual MCP service construction lives in
//! `gage-mcp` ([`gage_mcp::build_mcp_service`]); the claude child
//! spawn + event stream is the runtime's responsibility (steps 6.4/
//! 6.5 of the unify-llm-api work — currently stubbed).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;

use gage_agent::{AgentBuilder as GageAgentBuilder, AgentSession as GageAgentSession, SandboxSpec};
use gage_mcp::{CustomToolCallback, ServiceHandle, ToolSpec};
use rune::Any;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, FromValue, Mut, Object, Protocol, Value, VmError};
use rune::{ContextError, Module};
use serde_json::Value as JsonValue;

use crate::error::Error;
use crate::state::current_scan_ctx;

/// Builder produced by `call_agent(prompt)`. Accumulates per-call
/// configuration; `await` validates and turns it into an [`Agent`].
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CallAgent {
    #[rune(skip)]
    prompt: String,
    #[rune(skip)]
    model: Option<String>,
    #[rune(skip)]
    max_turns: Option<u32>,
    #[rune(skip)]
    timeout_secs: Option<u64>,
    #[rune(skip)]
    system_prompt: Option<String>,
    #[rune(skip)]
    append_system_prompt: Option<String>,
    /// Deferred parse outcome for `.gage_tools(...)`. Stored as a
    /// `Result` so we can surface argument-shape errors at `await`
    /// time rather than build time.
    #[rune(skip)]
    gage_tools: Option<super::Result<GageTools>>,
    #[rune(skip)]
    custom_tools: Option<super::Result<Vec<CustomToolDef>>>,
}

/// Resolved spec ready to feed into [`gage_mcp::ToolSpec`].
#[allow(dead_code)] // Fields consumed in 6.4 (claude spawn + MCP service registration).
pub(crate) struct CallSpec {
    pub prompt: String,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout_secs: Option<u64>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub gage_tools: GageTools,
    pub custom_tools: Vec<CustomToolDef>,
}

/// What the spec selects from the built-in Gage tool set.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Variant data consumed in 6.4.
pub(crate) enum GageTools {
    /// Caller omitted `.gage_tools(...)` entirely.
    None,
    /// Explicit list of tool names.
    Some(Vec<String>),
    /// `["*"]` — every built-in tool the host exposes.
    All,
}

/// One scanner-defined MCP tool. Mirrors the shape `SCANNER.tasks`
/// uses: keyed by Rune function name, the inner record carries the
/// wire-visible name, description, and input declaration.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Fields consumed in 6.4 (MCP tool route construction).
pub(crate) struct CustomToolDef {
    /// Rune function name that backs this tool (the dispatch key).
    pub fn_name: String,
    /// MCP tool name shown to the model.
    pub mcp_name: String,
    pub description: String,
    /// `inputs` shape from `SCANNER.tasks`. Parsed into a list so the
    /// caller can render it into a JSON schema later.
    pub inputs: Vec<InputDecl>,
}

/// One declared input on a scanner tool. Rendered into the MCP JSON
/// schema for the tool in 6.4.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Fields consumed in 6.4 (JSON schema rendering).
pub(crate) struct InputDecl {
    pub name: String,
    /// JSON Schema `type` (e.g. `"string"`, `"integer"`). Defaults to
    /// `"string"` when omitted.
    pub type_str: String,
    /// Defaults to `true` — every declared input is required unless
    /// `required: false` is set explicitly. Matches `call_llm`'s
    /// implicit semantics so the migration path is uniform.
    pub required: bool,
    pub description: Option<String>,
}

/// Handle to a running per-call agent. When the call declared any
/// tools (`.gage_tools(...)` or `.tools(...)`), an MCP service is
/// registered at `await` time and lives for the lifetime of this
/// value ([`ServiceHandle`] drops on `Drop`, unregistering from the
/// host). A call with no tools declared skips MCP entirely — the
/// agent runs pure text-in/text-out.
///
/// `session` is the spawned `claude -p` child. `poll`/event-stream
/// methods land in 6.4.d; today's `wait()` drives the child to
/// completion and returns an [`AgentResult`].
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Agent {
    #[rune(skip)]
    spec: Arc<CallSpec>,
    /// MCP URL the claude child connects to. `None` when no tools
    /// were declared.
    #[rune(skip)]
    mcp_url: Option<String>,
    /// Per-call MCP service registration. Drop unregisters. `None`
    /// when no tools were declared (no MCP server is running).
    #[rune(skip)]
    _service: Option<ServiceHandle>,
    /// Spawned `claude -p` child. Wrapped in `Mutex<Option<>>` so
    /// `wait()` can `.take()` it without requiring `&mut Agent` on a
    /// Rune-shared value — Rune `Mut<Agent>` does give us &mut, but
    /// `try_wait`/`running` use `&Agent`, and they need to peek at
    /// the session too.
    #[rune(skip)]
    session: Arc<Mutex<Option<GageAgentSession>>>,
}

/// What `wait()` returns. Today's fields are minimal — the full
/// design set (cost, usage, structured_output, etc.) lands in 6.6
/// when the stream-json parser surfaces them.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct AgentResult {
    /// Final assistant text from `claude -p`. Empty if the child
    /// exited without emitting a final message.
    #[rune(get)]
    pub text: String,
    /// Child exit code; `-1` if the process was signaled.
    #[rune(get, copy)]
    pub status: i64,
    /// Captured stderr — useful for diagnosing claude warnings that
    /// sit outside the JSON stream.
    #[rune(get)]
    pub stderr: String,
}

impl AgentResult {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AgentResult {{ status: {}, text: {:?}, stderr: {:?} }}",
            self.status, self.text, self.stderr
        )?;
        Ok(())
    }
}

impl Agent {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Agent {{ model: {:?}, max_turns: {:?}, gage_tools: {:?}, custom_tools: {}, url: {:?} }}",
            self.spec.model,
            self.spec.max_turns,
            self.spec.gage_tools,
            self.spec.custom_tools.len(),
            self.mcp_url,
        )?;
        Ok(())
    }
}

#[rune::function(instance)]
fn url(this: &Agent) -> Option<String> {
    this.mcp_url.clone()
}

#[rune::function(instance)]
async fn poll(_this: Mut<Agent>) -> super::Result<()> {
    Err(Error::Agent(
        "agent.poll(): not implemented yet (6.4)".into(),
    ))
}

#[rune::function(instance)]
async fn wait(this: Mut<Agent>) -> super::Result<AgentResult> {
    let session = this
        .session
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| Error::Agent("agent.wait(): session already consumed".into()))?;
    let mut session = session;
    let out = session
        .wait()
        .await
        .map_err(|e| Error::Agent(format!("agent.wait(): {e}")))?;
    Ok(AgentResult {
        text: String::from_utf8_lossy(&out.stdout).into_owned(),
        status: out.status.code().map(i64::from).unwrap_or(-1),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

#[rune::function(instance)]
fn try_wait(_this: &Agent) -> Option<()> {
    None
}

#[rune::function(instance)]
fn running(_this: &Agent) -> bool {
    false
}

#[rune::function(instance)]
async fn send(_this: Mut<Agent>, _msg: String) -> super::Result<()> {
    Err(Error::Agent(
        "agent.send(): not implemented yet (6.5)".into(),
    ))
}

#[rune::function(instance)]
async fn send_now(_this: Mut<Agent>, _msg: String) -> super::Result<()> {
    Err(Error::Agent(
        "agent.send_now(): not implemented yet (6.5)".into(),
    ))
}

#[rune::function(instance)]
async fn stop(_this: Mut<Agent>, _grace_secs: i64) -> super::Result<()> {
    Err(Error::Agent(
        "agent.stop(): not implemented yet (6.5)".into(),
    ))
}

#[rune::function(instance)]
async fn kill(_this: Mut<Agent>, _grace_secs: i64) -> super::Result<()> {
    Err(Error::Agent(
        "agent.kill(): not implemented yet (6.5)".into(),
    ))
}

impl CallAgent {
    #[rune::function(instance)]
    fn model(mut self, model: String) -> Self {
        self.model = Some(model);
        self
    }

    #[rune::function(instance)]
    fn max_turns(mut self, max_turns: i64) -> Self {
        self.max_turns = Some(max_turns.max(0) as u32);
        self
    }

    #[rune::function(instance)]
    fn timeout(mut self, timeout: i64) -> Self {
        self.timeout_secs = Some(timeout.max(0) as u64);
        self
    }

    #[rune::function(instance)]
    fn system_prompt(mut self, s: String) -> Self {
        self.system_prompt = Some(s);
        self
    }

    #[rune::function(instance)]
    fn append_system_prompt(mut self, s: String) -> Self {
        self.append_system_prompt = Some(s);
        self
    }

    #[rune::function(instance)]
    fn gage_tools(mut self, list: Value) -> Self {
        self.gage_tools = Some(parse_gage_tools(list));
        self
    }

    #[rune::function(instance)]
    fn tools(mut self, map: Value) -> Self {
        self.custom_tools = Some(parse_custom_tools(map));
        self
    }
}

fn parse_gage_tools(v: Value) -> super::Result<GageTools> {
    let items: rune::runtime::Vec = FromValue::from_value(v)
        .map_err(|e| Error::Agent(format!("'gage_tools' must be a list of strings: {e}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        let s: String = FromValue::from_value(item.clone())
            .map_err(|e| Error::Agent(format!("'gage_tools' must be a list of strings: {e}")))?;
        out.push(s);
    }
    if out.iter().any(|s| s == "*") {
        if out.len() != 1 {
            return Err(Error::Agent(
                "'gage_tools': \"*\" must be the only entry".into(),
            ));
        }
        return Ok(GageTools::All);
    }
    Ok(GageTools::Some(out))
}

fn parse_custom_tools(v: Value) -> super::Result<Vec<CustomToolDef>> {
    let obj: Object = FromValue::from_value(v).map_err(|e| {
        Error::Agent(format!(
            "'tools' must be an object keyed by function name: {e}"
        ))
    })?;
    // Sort by key so the resulting tool order is deterministic.
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (k, val) in obj.iter() {
        entries.insert(k.to_string(), val.clone());
    }
    let mut out = Vec::with_capacity(entries.len());
    for (fn_name, decl) in entries {
        out.push(parse_one_custom_tool(fn_name, decl)?);
    }
    Ok(out)
}

fn parse_one_custom_tool(fn_name: String, decl: Value) -> super::Result<CustomToolDef> {
    let obj: Object = FromValue::from_value(decl).map_err(|e| {
        Error::Agent(format!(
            "'tools.{fn_name}' must be an object {{ name, description, inputs }}: {e}"
        ))
    })?;
    let mcp_name = pop_string(&obj, "name", &fn_name)?;
    let description = pop_string(&obj, "description", &fn_name)?;
    let inputs = match obj.get(&rune::alloc::String::try_from("inputs").unwrap()) {
        Some(v) => parse_inputs(v.clone(), &fn_name)?,
        None => Vec::new(),
    };
    Ok(CustomToolDef {
        fn_name,
        mcp_name,
        description,
        inputs,
    })
}

fn pop_string(obj: &Object, key: &str, fn_name: &str) -> super::Result<String> {
    let v = obj
        .get(&rune::alloc::String::try_from(key).unwrap())
        .ok_or_else(|| Error::Agent(format!("'tools.{fn_name}.{key}' is required")))?;
    String::from_value(v.clone())
        .map_err(|e| Error::Agent(format!("'tools.{fn_name}.{key}' must be a string: {e}")))
}

fn parse_inputs(v: Value, fn_name: &str) -> super::Result<Vec<InputDecl>> {
    let obj: Object = FromValue::from_value(v).map_err(|e| {
        Error::Agent(format!(
            "'tools.{fn_name}.inputs' must be an object keyed by input name: {e}"
        ))
    })?;
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for (k, val) in obj.iter() {
        entries.insert(k.to_string(), val.clone());
    }
    let mut out = Vec::with_capacity(entries.len());
    for (name, decl) in entries {
        out.push(parse_one_input(fn_name, name, decl)?);
    }
    Ok(out)
}

fn parse_one_input(fn_name: &str, name: String, decl: Value) -> super::Result<InputDecl> {
    let obj: Object = FromValue::from_value(decl).map_err(|e| {
        Error::Agent(format!(
            "'tools.{fn_name}.inputs.{name}' must be an object \
             {{ type, required?, description? }}: {e}"
        ))
    })?;
    let type_str = match obj.get(&rune::alloc::String::try_from("type").unwrap()) {
        Some(v) => String::from_value(v.clone()).map_err(|e| {
            Error::Agent(format!(
                "'tools.{fn_name}.inputs.{name}.type' must be a string: {e}"
            ))
        })?,
        None => "string".to_string(),
    };
    let required = match obj.get(&rune::alloc::String::try_from("required").unwrap()) {
        Some(v) => bool::from_value(v.clone()).map_err(|e| {
            Error::Agent(format!(
                "'tools.{fn_name}.inputs.{name}.required' must be a bool: {e}"
            ))
        })?,
        None => true,
    };
    let description = match obj.get(&rune::alloc::String::try_from("description").unwrap()) {
        Some(v) => Some(String::from_value(v.clone()).map_err(|e| {
            Error::Agent(format!(
                "'tools.{fn_name}.inputs.{name}.description' must be a string: {e}"
            ))
        })?),
        None => None,
    };
    Ok(InputDecl {
        name,
        type_str,
        required,
        description,
    })
}

#[rune::function]
fn call_agent(prompt: String) -> CallAgent {
    CallAgent {
        prompt,
        model: None,
        max_turns: None,
        timeout_secs: None,
        system_prompt: None,
        append_system_prompt: None,
        gage_tools: None,
        custom_tools: None,
    }
}

async fn do_call_agent(c: CallAgent) -> super::Result<Agent> {
    let CallAgent {
        prompt,
        model,
        max_turns,
        timeout_secs,
        system_prompt,
        append_system_prompt,
        gage_tools,
        custom_tools,
    } = c;
    let gage_tools = match gage_tools {
        Some(r) => r?,
        None => GageTools::None,
    };
    let custom_tools = match custom_tools {
        Some(r) => r?,
        None => Vec::new(),
    };
    let spec = Arc::new(CallSpec {
        prompt,
        model,
        max_turns,
        timeout_secs,
        system_prompt,
        append_system_prompt,
        gage_tools,
        custom_tools,
    });

    let tool_spec = build_tool_spec(&spec);
    let gage_tools_resolved = tool_spec.gage_tools.clone();
    let (mcp_url, service) = if tool_spec.gage_tools.is_empty() && tool_spec.custom_tools.is_empty()
    {
        // Pure text-in/text-out call — no need to register an MCP
        // service or even reach the host.
        (None, None)
    } else {
        let ctx = current_scan_ctx();
        let host = ctx.run.mcp_host.clone().ok_or_else(|| {
            Error::Agent(
                "call_agent: tools declared but no MCP host available in this run \
                 (host startup failed?)"
                    .into(),
            )
        })?;
        let svc = host.register(gage_mcp::build_mcp_service(tool_spec));
        (Some(svc.url().to_string()), Some(svc))
    };

    let scan_id = current_scan_ctx().run.scan_id.clone();
    let sandbox = scan_sandbox_spec(&scan_id).map_err(Error::Agent)?;
    // `settings.json` permissions.allow must include both built-in
    // Gage tools and scanner-defined custom tools — otherwise claude
    // prompts the user on every custom-tool call, which in `-p`
    // headless mode means the call is rejected.
    let mut auto_allow = gage_tools_resolved;
    for def in &spec.custom_tools {
        auto_allow.push(def.mcp_name.clone());
    }
    let mut builder = GageAgentBuilder::new()
        .tools(auto_allow)
        .sandbox(sandbox)
        .scan_id(&scan_id);
    if let Some(url) = &mcp_url {
        builder = builder.mcp_url(url.clone());
    }
    if let Some(m) = &spec.model {
        // Preserve `call_llm`'s alias support — `"small"`/`"medium"`/
        // `"large"` round-trip to concrete claude model ids so existing
        // scanner code keeps working unchanged.
        builder = builder.model(crate::llm::anthropic::resolve_model(m).to_string());
    }
    if let Some(n) = spec.max_turns {
        builder = builder.max_turns(n);
    }
    if let Some(t) = spec.timeout_secs {
        builder = builder.timeout(t as usize);
    }
    let session = builder
        .build()
        .start_session(&spec.prompt)
        .await
        .map_err(|e| Error::Agent(format!("call_agent: start_session: {e}")))?;

    Ok(Agent {
        spec,
        mcp_url,
        _service: service,
        session: Arc::new(Mutex::new(Some(session))),
    })
}

/// Restrict the agent's sandbox to the running scan's rows. Reads the
/// canonical gage db once for the scan-session / scan-note / scan-issue
/// sets that already exist.
fn scan_sandbox_spec(scan_id: &str) -> Result<SandboxSpec, String> {
    let conn = gage_db::db::open_db().map_err(|e| e.to_string())?;
    let sessions: HashSet<String> = gage_db::scan::session_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let notes: HashSet<String> = gage_db::scan::note_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    let issues: HashSet<String> = gage_db::scan::issue_ids_for_scan(&conn, scan_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    Ok(SandboxSpec {
        sessions: Some(sessions),
        notes: Some(notes),
        issues: Some(issues),
        scan: Some(scan_id.to_string()),
    })
}

/// Translate the resolved [`CallSpec`] into [`gage_mcp::ToolSpec`].
///
/// `GageTools::All` expands against [`gage_agent::TOOL_NAMES`];
/// `GageTools::Some(list)` passes through verbatim; `GageTools::None`
/// yields an empty `gage_tools`. Each custom tool becomes a
/// [`gage_mcp::CustomToolDef`] whose callback spawns a fresh
/// `rune::Vm` over the calling task's compiled unit, scoped to the
/// same `ScanContext` — see "Scanner-tool callback execution" in the
/// design doc.
fn build_tool_spec(spec: &CallSpec) -> ToolSpec {
    let gage_tools = match &spec.gage_tools {
        GageTools::None => Vec::new(),
        GageTools::Some(list) => list.clone(),
        GageTools::All => gage_agent::TOOL_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let ctx = current_scan_ctx();
    let dispatcher_sender = ctx.run.dispatcher.get().map(|d| d.sender());
    let module_id = ctx.scanner_name.clone();
    let custom_tools = spec
        .custom_tools
        .iter()
        .map(|def| gage_mcp::CustomToolDef {
            name: def.mcp_name.clone(),
            description: def.description.clone(),
            input_schema: render_input_schema(&def.inputs),
            callback: dispatcher_callback(
                module_id.clone(),
                def.fn_name.clone(),
                dispatcher_sender.clone(),
            ),
        })
        .collect();
    ToolSpec {
        gage_tools,
        custom_tools,
    }
}

/// Render `InputDecl[]` to the MCP `{"type": "object", "properties":
/// {...}, "required": [...]}` schema.
fn render_input_schema(inputs: &[InputDecl]) -> rmcp_json::JsonObject {
    let mut properties = rmcp_json::JsonObject::new();
    let mut required: Vec<JsonValue> = Vec::new();
    for inp in inputs {
        let mut prop = rmcp_json::JsonObject::new();
        prop.insert("type".into(), JsonValue::String(inp.type_str.clone()));
        if let Some(desc) = &inp.description {
            prop.insert("description".into(), JsonValue::String(desc.clone()));
        }
        properties.insert(inp.name.clone(), JsonValue::Object(prop));
        if inp.required {
            required.push(JsonValue::String(inp.name.clone()));
        }
    }
    let mut obj = rmcp_json::JsonObject::new();
    obj.insert("type".into(), JsonValue::String("object".into()));
    obj.insert("properties".into(), JsonValue::Object(properties));
    obj.insert("required".into(), JsonValue::Array(required));
    obj
}

/// Build the MCP callback for a scanner-defined tool. The closure
/// sends a JSON-only [`DispatchRequest`] to the dispatcher and waits
/// for the reply — no Rune state crosses the channel.
fn dispatcher_callback(
    module_id: String,
    fn_name: String,
    sender: Option<tokio::sync::mpsc::UnboundedSender<crate::dispatcher::DispatchRequest>>,
) -> CustomToolCallback {
    Arc::new(move |args| {
        let module_id = module_id.clone();
        let fn_name = fn_name.clone();
        let sender = sender.clone();
        Box::pin(async move {
            let sender = sender.ok_or_else(|| {
                "tool dispatcher unavailable for this run (startup failed?)".to_string()
            })?;
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            sender
                .send(crate::dispatcher::DispatchRequest {
                    module_id,
                    fn_name,
                    args,
                    reply: reply_tx,
                })
                .map_err(|_send_err| "tool dispatcher channel closed".to_string())?;
            reply_rx
                .await
                .map_err(|_recv_err| "tool dispatcher dropped reply".to_string())?
        })
    })
}

/// `rmcp::model::JsonObject` re-exported via `gage_mcp::CustomToolDef`'s
/// `input_schema` field type — alias kept local so this module doesn't
/// import the rmcp crate just for one type name.
mod rmcp_json {
    pub type JsonObject = serde_json::Map<String, serde_json::Value>;
}

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Agent>()?;
    m.function_meta(Agent::debug)?;
    m.ty::<AgentResult>()?;
    m.function_meta(AgentResult::debug)?;
    m.function_meta(url)?;
    m.function_meta(poll)?;
    m.function_meta(wait)?;
    m.function_meta(try_wait)?;
    m.function_meta(running)?;
    m.function_meta(send)?;
    m.function_meta(send_now)?;
    m.function_meta(stop)?;
    m.function_meta(kill)?;

    m.ty::<CallAgent>()?;
    m.function_meta(CallAgent::model)?;
    m.function_meta(CallAgent::max_turns)?;
    m.function_meta(CallAgent::timeout)?;
    m.function_meta(CallAgent::system_prompt)?;
    m.function_meta(CallAgent::append_system_prompt)?;
    m.function_meta(CallAgent::gage_tools)?;
    m.function_meta(CallAgent::tools)?;
    m.function_meta(call_agent)?;
    m.associated_function(&Protocol::INTO_FUTURE, |c: CallAgent| async move {
        do_call_agent(c).await
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use rune::Module;

    #[test]
    fn registers_into_module() {
        let mut m = Module::with_crate("gage").unwrap();
        super::register(&mut m).unwrap();
    }
}
