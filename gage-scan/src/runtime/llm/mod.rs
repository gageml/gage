pub(crate) mod anthropic;

use std::collections::VecDeque;
use std::sync::Mutex;

use rune::alloc::fmt::TryWrite;
use rune::alloc::prelude::TryClone;
use rune::runtime::{Formatter, Object, Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};
use serde_json as json;
use tracing::debug;

use crate::runtime::error::Error;
use crate::runtime::value::json_to_object;

use anthropic::{
    CacheControl, Client, ContentBlock, HttpClient, LlmError, Message as ApiMessage,
    MessagesRequest, Role, StopReason as ApiStopReason, SystemBlock, ToolDef,
};

const DEFAULT_MODEL: &str = "sonnet";
const DEFAULT_MAX_ROUNDS: u32 = 8;
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_SYSTEM_PROMPT: &str = "You are an analysis tool. Use the provided tools to report \
    findings. When done, return without calling further tools.";

#[derive(Clone, Copy, Any)]
#[rune(item = ::gage)]
pub(crate) enum StopReason {
    #[rune(constructor)]
    EndTurn,
    #[rune(constructor)]
    MaxTokens,
    #[rune(constructor)]
    StopSequence,
    #[rune(constructor)]
    PauseTurn,
    #[rune(constructor)]
    Refusal,
    #[rune(constructor)]
    MaxRounds,
}

impl StopReason {
    fn label(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "EndTurn",
            StopReason::MaxTokens => "MaxTokens",
            StopReason::StopSequence => "StopSequence",
            StopReason::PauseTurn => "PauseTurn",
            StopReason::Refusal => "Refusal",
            StopReason::MaxRounds => "MaxRounds",
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "{}", self.label())?;
        Ok(())
    }
}

impl TryClone for StopReason {
    fn try_clone(&self) -> Result<Self, rune::alloc::Error> {
        Ok(*self)
    }
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) enum PollResult {
    #[rune(constructor)]
    Tool(#[rune(get)] Value, #[rune(get)] Value, #[rune(get)] Value),
    #[rune(constructor)]
    Text(#[rune(get)] Value),
    #[rune(constructor)]
    Stop(#[rune(get)] StopReason),
}

impl PollResult {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        match self {
            PollResult::Tool(name, _, _) => write!(f, "Tool({name:?})")?,
            PollResult::Text(text) => write!(f, "Text({text:?})")?,
            PollResult::Stop(reason) => write!(f, "Stop({})", reason.label())?,
        }
        Ok(())
    }
}

struct SessionInner {
    client: std::sync::Arc<HttpClient>,
    model: String,
    max_tokens: u32,
    max_rounds: u32,
    system: Vec<SystemBlock>,
    tool_defs: Vec<ToolDef>,
    messages: Vec<ApiMessage>,
    pending_blocks: VecDeque<ContentBlock>,
    pending_tool_results: Vec<ContentBlock>,
    api_stop_reason: Option<ApiStopReason>,
    round: u32,
    is_active: bool,
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct LlmSession {
    #[rune(skip)]
    inner: std::sync::Arc<Mutex<SessionInner>>,
}

impl LlmSession {
    #[rune::function(instance)]
    fn active(&self) -> bool {
        self.inner.lock().unwrap().is_active
    }

    #[rune::function(instance)]
    fn tool_result(&self, id: String, result: Value) {
        let (content, is_error) = interpret_tool_result(result);
        self.inner
            .lock()
            .unwrap()
            .pending_tool_results
            .push(ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                is_error,
            });
    }
}

#[rune::function(instance)]
async fn poll(session: Ref<LlmSession>) -> super::Result<PollResult> {
    let inner = session.inner.clone();
    do_poll(inner).await
}

async fn do_poll(inner: std::sync::Arc<Mutex<SessionInner>>) -> super::Result<PollResult> {
    {
        let mut state = inner.lock().unwrap();
        if let Some(block) = state.pending_blocks.pop_front() {
            return Ok(to_poll_result(block));
        }
        if state.api_stop_reason != Some(ApiStopReason::ToolUse) {
            state.is_active = false;
            return Ok(PollResult::Stop(to_stop_reason(state.api_stop_reason)));
        }
        if state.round >= state.max_rounds {
            state.is_active = false;
            return Ok(PollResult::Stop(StopReason::MaxRounds));
        }
    }

    send_next_round(&inner).await?;

    let mut state = inner.lock().unwrap();
    if let Some(block) = state.pending_blocks.pop_front() {
        return Ok(to_poll_result(block));
    }
    state.is_active = false;
    Ok(PollResult::Stop(to_stop_reason(state.api_stop_reason)))
}

async fn send_next_round(inner: &std::sync::Arc<Mutex<SessionInner>>) -> super::Result<()> {
    let (client, req) = {
        let mut state = inner.lock().unwrap();
        let mut results = std::mem::take(&mut state.pending_tool_results);
        let handled: std::collections::HashSet<String> = results
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        if let Some(last) = state.messages.last() {
            for block in &last.content {
                if let ContentBlock::ToolUse { id, .. } = block
                    && !handled.contains(id)
                {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: "Bad request: no such tool".to_string(),
                        is_error: true,
                    });
                }
            }
        }
        state.messages.push(ApiMessage {
            role: Role::User,
            content: results,
        });
        state.round += 1;

        let req = MessagesRequest {
            model: state.model.clone(),
            max_tokens: state.max_tokens,
            system: state.system.clone(),
            messages: state.messages.clone(),
            tools: state.tool_defs.clone(),
        };
        (state.client.clone(), req)
    };

    let resp = client.messages(&req).await?;

    let mut state = inner.lock().unwrap();
    debug!(
        round = state.round,
        stop_reason = ?resp.stop_reason,
        input_tokens = resp.usage.input_tokens,
        output_tokens = resp.usage.output_tokens,
        "llm round"
    );
    state.messages.push(ApiMessage {
        role: Role::Assistant,
        content: resp.content.clone(),
    });
    state.pending_blocks = resp.content.into_iter().collect();
    state.api_stop_reason = Some(resp.stop_reason);

    Ok(())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CallLlm {
    #[rune(skip)]
    prompt: String,
    #[rune(skip)]
    model: Option<String>,
    #[rune(skip)]
    max_rounds: Option<u32>,
    #[rune(skip)]
    system_prompt: Option<String>,
    #[rune(skip)]
    tools: Vec<ToolDef>,
}

impl CallLlm {
    #[rune::function(instance)]
    fn model(mut self, model: String) -> Self {
        self.model = Some(model);
        self
    }

    #[rune::function(instance)]
    fn max_rounds(mut self, max_rounds: i64) -> Self {
        self.max_rounds = Some(max_rounds.max(0) as u32);
        self
    }

    #[rune::function(instance)]
    fn system_prompt(mut self, system_prompt: String) -> Self {
        self.system_prompt = Some(system_prompt);
        self
    }

    #[rune::function(instance)]
    fn tool(mut self, name: String, def: Object) -> Self {
        let description = opt_string(&def, "description").unwrap_or_default();
        let input_schema = parse_tool_inputs(&def);
        self.tools.push(ToolDef {
            name,
            description,
            input_schema,
        });
        self
    }
}

#[rune::function]
fn llm(prompt: String) -> CallLlm {
    CallLlm {
        prompt,
        model: None,
        max_rounds: None,
        system_prompt: None,
        tools: Vec::new(),
    }
}

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<StopReason>()?;
    m.function_meta(StopReason::debug)?;
    m.ty::<PollResult>()?;
    m.function_meta(PollResult::debug)?;
    m.ty::<LlmSession>()?;

    m.function_meta(LlmSession::active)?;
    m.function_meta(LlmSession::tool_result)?;
    m.function_meta(poll)?;

    m.ty::<CallLlm>()?;
    m.function_meta(CallLlm::model)?;
    m.function_meta(CallLlm::max_rounds)?;
    m.function_meta(CallLlm::system_prompt)?;
    m.function_meta(CallLlm::tool)?;
    m.function_meta(llm)?;
    m.associated_function(&Protocol::INTO_FUTURE, |c: CallLlm| async move {
        do_call_llm(c).await
    })?;

    Ok(())
}

async fn do_call_llm(c: CallLlm) -> super::Result<LlmSession> {
    let model_alias = c.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let model = anthropic::resolve_model(&model_alias).to_string();

    let max_rounds = c.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS);

    let tool_defs = c.tools;
    let client = std::sync::Arc::new(HttpClient::from_env()?);

    let system_text = c
        .system_prompt
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    let system = vec![SystemBlock::text_cached(system_text)];

    let messages = vec![ApiMessage {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: c.prompt,
            cache_control: Some(CacheControl::ephemeral()),
        }],
    }];

    debug!(model = model.as_str(), tools = tool_defs.len(), "llm");

    let req = MessagesRequest {
        model: model.clone(),
        max_tokens: DEFAULT_MAX_TOKENS,
        system: system.clone(),
        messages: messages.clone(),
        tools: tool_defs.clone(),
    };

    let resp = client.messages(&req).await?;

    debug!(
        round = 1,
        stop_reason = ?resp.stop_reason,
        input_tokens = resp.usage.input_tokens,
        output_tokens = resp.usage.output_tokens,
        "llm round"
    );

    let mut all_messages = messages;
    all_messages.push(ApiMessage {
        role: Role::Assistant,
        content: resp.content.clone(),
    });

    let pending_blocks: VecDeque<ContentBlock> = resp.content.into_iter().collect();
    let is_active = !pending_blocks.is_empty() || resp.stop_reason == ApiStopReason::ToolUse;

    Ok(LlmSession {
        inner: std::sync::Arc::new(Mutex::new(SessionInner {
            client,
            model,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_rounds,
            system,
            tool_defs,
            messages: all_messages,
            pending_blocks,
            pending_tool_results: Vec::new(),
            api_stop_reason: Some(resp.stop_reason),
            round: 1,
            is_active,
        })),
    })
}

fn parse_tool_inputs(tool: &Object) -> json::Value {
    let inputs_val = match tool.get("inputs") {
        Some(v) => v.clone(),
        None => return json::json!({"type": "object", "properties": {}}),
    };

    let inputs: Vec<Value> = rune::from_value(inputs_val).unwrap_or_default();
    let mut properties = json::Map::new();
    let mut required = Vec::new();

    for input_val in &inputs {
        let input_obj: Object =
            rune::from_value(input_val.clone()).expect("tool input must be an object");

        let name = opt_string(&input_obj, "name").expect("tool input missing 'name'");

        let type_str = opt_string(&input_obj, "type").unwrap_or_else(|| "string".to_string());

        let mut prop = json::Map::new();
        prop.insert("type".into(), json::Value::String(type_str));
        if let Some(desc) = opt_string(&input_obj, "description") {
            prop.insert("description".into(), json::Value::String(desc));
        }

        properties.insert(name.clone(), json::Value::Object(prop));
        required.push(json::Value::String(name));
    }

    json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

fn opt_string(obj: &Object, field: &str) -> Option<String> {
    obj.get(field).map(|v| {
        v.borrow_string_ref()
            .unwrap_or_else(|_| panic!("tool field '{field}' must be a string"))
            .to_string()
    })
}

fn to_poll_result(block: ContentBlock) -> PollResult {
    match block {
        ContentBlock::ToolUse { id, name, input } => {
            let args_obj = match input {
                json::Value::Object(_) => json_to_object(&input),
                _ => Object::new(),
            };
            PollResult::Tool(
                rune::to_value(name).unwrap(),
                rune::to_value(args_obj).unwrap(),
                rune::to_value(id).unwrap(),
            )
        }
        ContentBlock::Text { text, .. } => PollResult::Text(rune::to_value(text).unwrap()),
        ContentBlock::ToolResult { .. } => PollResult::Text(rune::to_value(String::new()).unwrap()),
    }
}

fn to_stop_reason(reason: Option<ApiStopReason>) -> StopReason {
    match reason {
        Some(ApiStopReason::EndTurn) | None => StopReason::EndTurn,
        Some(ApiStopReason::MaxTokens) => StopReason::MaxTokens,
        Some(ApiStopReason::StopSequence) => StopReason::StopSequence,
        Some(ApiStopReason::PauseTurn) => StopReason::PauseTurn,
        Some(ApiStopReason::Refusal) => StopReason::Refusal,
        Some(ApiStopReason::ToolUse) => StopReason::EndTurn,
    }
}

fn interpret_tool_result(result: Value) -> (String, bool) {
    match rune::from_value::<Result<Value, Value>>(result.clone()) {
        Ok(Ok(v)) => (value_to_string(&v), false),
        Ok(Err(v)) => (value_to_string(&v), true),
        Err(_) => (value_to_string(&result), false),
    }
}

fn value_to_string(v: &Value) -> String {
    v.borrow_string_ref()
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "ok".to_string())
}

impl From<LlmError> for Error {
    fn from(e: LlmError) -> Self {
        match e {
            LlmError::Config(msg) => Error::Config(msg),
            LlmError::Network(msg) => Error::Network(msg),
            LlmError::Http { status, body } => Error::Http {
                status: i64::from(status),
                body,
            },
            LlmError::Decode(msg) => Error::Decode(msg),
        }
    }
}
