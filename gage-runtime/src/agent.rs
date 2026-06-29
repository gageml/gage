//! `call_agent(prompt)` — Rune builder that, on await, spawns an
//! isolated `claude -p` child driven by an in-process MCP service
//! exposing a per-call tool set, and returns an [`Agent`] handle.
//!
//! This module owns the Rune-visible surface and the parsing of
//! builder arguments. The actual MCP service construction lives in
//! `gage-mcp` ([`gage_mcp::build_mcp_service`]); the claude child
//! spawn + event stream is the runtime's responsibility (steps 6.4/
//! 6.5 of the unify-llm-api work — currently stubbed).

use std::collections::BTreeMap;
use std::sync::Arc;

use rune::Any;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, FromValue, Mut, Object, Protocol, Value, VmError};
use rune::{ContextError, Module};

use crate::error::Error;

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

/// Handle to a running per-call agent. Methods (`poll`/`wait`/etc.)
/// are stubbed in this sub-step; the live wiring (claude child, MCP
/// service registration) is added in 6.4/6.5.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Agent {
    #[rune(skip)]
    spec: Arc<CallSpec>,
}

impl Agent {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Agent {{ model: {:?}, max_turns: {:?}, gage_tools: {:?}, custom_tools: {} }}",
            self.spec.model,
            self.spec.max_turns,
            self.spec.gage_tools,
            self.spec.custom_tools.len()
        )?;
        Ok(())
    }
}

#[rune::function(instance)]
async fn poll(_this: Mut<Agent>) -> super::Result<()> {
    Err(Error::Agent(
        "agent.poll(): not implemented yet (6.4)".into(),
    ))
}

#[rune::function(instance)]
async fn wait(_this: Mut<Agent>) -> super::Result<()> {
    Err(Error::Agent(
        "agent.wait(): not implemented yet (6.4)".into(),
    ))
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
    let spec = CallSpec {
        prompt,
        model,
        max_turns,
        timeout_secs,
        system_prompt,
        append_system_prompt,
        gage_tools,
        custom_tools,
    };
    Ok(Agent {
        spec: Arc::new(spec),
    })
}

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Agent>()?;
    m.function_meta(Agent::debug)?;
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
