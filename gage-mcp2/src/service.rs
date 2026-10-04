//! Build an HTTP MCP service from a [`ToolSpec`].
//!
//! The spec names the Gage tools to expose, each with the data it
//! serves, and the scanner-defined tools, each with the callback that
//! runs it. Both kinds surface as ordinary MCP tools. [`build_mcp_service`]
//! returns the [`RegisteredService`] shape a [`crate::host::McpHost`]
//! expects.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use datafusion::prelude::SessionContext;
use rmcp::ErrorData;
use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, Content, JsonObject, Tool as ToolMeta, ToolAnnotations};
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use tower::service_fn;
use tower::util::BoxCloneSyncService;

use crate::host::RegisteredService;
use crate::server::GageServer;
use crate::tools;

/// What one service exposes
#[derive(Default)]
pub struct ToolSpec {
    pub gage: Vec<GageTool>,
    pub custom: Vec<CustomToolDef>,
}

/// One Gage tool with the data it serves
#[derive(Clone)]
pub enum GageTool {
    Query(QueryConfig),
    IssueWrite(IssueWriteConfig),
}

impl GageTool {
    /// The tool's wire name
    pub fn name(&self) -> &'static str {
        match self {
            GageTool::Query(_) => tools::query::NAME,
            GageTool::IssueWrite(_) => tools::issue_write::NAME,
        }
    }
}

/// The `Query` tool's data: the DataFusion context its SQL runs on
#[derive(Clone)]
pub struct QueryConfig {
    pub context: Arc<SessionContext>,
}

/// The `IssueWrite` tool's data: the callback that writes the issue.
/// The write path belongs to the runtime, which stages issues in the
/// scan directory, so the tool carries the writer rather than the
/// data; the route parses the call and hands the callback an
/// [`IssueWriteInput`].
#[derive(Clone)]
pub struct IssueWriteConfig {
    pub callback: IssueWriteCallback,
}

/// Async closure that writes one issue from a parsed call.
pub type IssueWriteCallback = Arc<
    dyn Fn(IssueWriteInput) -> Pin<Box<dyn Future<Output = CustomToolOutcome> + Send>>
        + Send
        + Sync,
>;

/// One `IssueWrite` call, parsed
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueWriteInput {
    pub title: String,
    /// `None` when absent or empty
    pub description: Option<String>,
    /// The cited note ids, in call order
    pub evidence: Vec<String>,
}

/// The configuration the Gage tool handlers read through
/// [`GageServer`]. A tool absent from the spec has no entry and no
/// route.
#[derive(Clone, Default)]
pub struct ToolsConfig {
    pub query: Option<QueryConfig>,
    pub issue_write: Option<IssueWriteConfig>,
}

/// One scanner-defined tool: the wire-visible definition plus the
/// callback that runs when the model calls it.
pub struct CustomToolDef {
    pub name: String,
    /// Omitted from the wire definition when `None`
    pub description: Option<String>,
    /// JSON Schema for the tool's input, in the object form MCP expects
    pub input_schema: JsonObject,
    pub annotations: Option<ToolAnnotations>,
    pub callback: CustomToolCallback,
}

/// Async closure invoked when the model calls a [`CustomToolDef`].
/// Receives the call's `arguments` object and the request's `_meta`
/// object as JSON values.
pub type CustomToolCallback = Arc<
    dyn Fn(Value, Value) -> Pin<Box<dyn Future<Output = CustomToolOutcome> + Send>> + Send + Sync,
>;

/// What a [`CustomToolCallback`] produced. MCP reports a tool call at
/// two levels: a result the model reads, which may be an error the
/// model can act on, and a protocol error for a tool that is broken.
#[derive(Debug, Clone, PartialEq)]
pub enum CustomToolOutcome {
    /// The tool result. A string is sent as is; any other value is
    /// sent as JSON text.
    Success(Value),
    /// A tool result with `isError: true` carrying this text
    Error(String),
    /// A JSON-RPC internal error carrying this text
    Fault(String),
}

/// Construct a streamable-HTTP MCP service exposing every tool the
/// spec declares. The returned service plugs into a
/// [`crate::host::McpHost`] via `McpHost::register`.
pub fn build_mcp_service(spec: ToolSpec) -> RegisteredService {
    let inner = Arc::new(StreamableHttpService::new(
        move || Ok(build_server(&spec)),
        LocalSessionManager::default().into(),
        Default::default(),
    ));
    let svc = service_fn(move |req| {
        let inner = Arc::clone(&inner);
        async move { Ok::<_, Infallible>(inner.handle(req).await) }
    });
    BoxCloneSyncService::new(svc)
}

fn build_server(spec: &ToolSpec) -> GageServer {
    let mut router = ToolRouter::<GageServer>::new();
    let mut config = ToolsConfig::default();
    for tool in &spec.gage {
        match tool {
            GageTool::Query(c) => {
                config.query = Some(c.clone());
                router = router.with_route((tools::query::TOOL)());
            }
            GageTool::IssueWrite(c) => {
                config.issue_write = Some(c.clone());
                router = router.with_route((tools::issue_write::TOOL)());
            }
        }
    }
    for def in &spec.custom {
        router = router.with_route(custom_route(def));
    }
    GageServer::new(router, config)
}

fn custom_route(def: &CustomToolDef) -> ToolRoute<GageServer> {
    let meta = ToolMeta {
        name: def.name.clone().into(),
        title: None,
        description: def.description.clone().map(Into::into),
        input_schema: Arc::new(def.input_schema.clone()),
        output_schema: None,
        annotations: def.annotations.clone(),
        execution: None,
        icons: None,
        meta: None,
    };
    let callback = Arc::clone(&def.callback);
    ToolRoute::new_dyn(meta, move |ctx: ToolCallContext<'_, GageServer>| {
        let args = ctx
            .arguments
            .clone()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        // rmcp moves the request's `_meta` into the request context
        // before dispatch (the params-level `meta` field arrives
        // emptied).
        let meta = Value::Object(ctx.request_context.meta.0.clone());
        let callback = Arc::clone(&callback);
        Box::pin(async move { call_result((callback)(args, meta).await) })
    })
}

/// The wire form of a callback's outcome: a success or error result
/// the model reads, or a JSON-RPC internal error for a fault.
pub(crate) fn call_result(outcome: CustomToolOutcome) -> Result<CallToolResult, ErrorData> {
    match outcome {
        CustomToolOutcome::Success(out) => Ok(CallToolResult::success(vec![Content::text(
            render_output(&out),
        )])),
        CustomToolOutcome::Error(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        CustomToolOutcome::Fault(e) => Err(ErrorData::internal_error(e, None)),
    }
}

/// Render a callback's JSON return value for the tool result. Strings
/// pass through unquoted; everything else is JSON-stringified so the
/// model sees structured data verbatim.
fn render_output(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    #[test]
    fn success_string_is_sent_as_is_and_other_values_as_json() {
        let r = call_result(CustomToolOutcome::Success(json!("plain"))).unwrap();
        assert_eq!(r.is_error, Some(false));
        assert_eq!(text_of(&r), "plain");
        let r = call_result(CustomToolOutcome::Success(json!({"a": [1, 2]}))).unwrap();
        assert_eq!(text_of(&r), r#"{"a":[1,2]}"#);
    }

    #[test]
    fn error_is_a_result_the_model_reads() {
        let r = call_result(CustomToolOutcome::Error("bad key".into())).unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r), "bad key");
    }

    #[test]
    fn fault_is_a_protocol_error() {
        let e = call_result(CustomToolOutcome::Fault("handler broke".into())).unwrap_err();
        assert_eq!(e.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert_eq!(e.message, "handler broke");
    }

    #[test]
    fn builds_a_service_with_both_tool_kinds() {
        let mut schema = JsonObject::new();
        schema.insert("type".into(), Value::String("object".into()));
        let spec = ToolSpec {
            gage: vec![GageTool::Query(QueryConfig {
                context: Arc::new(SessionContext::new()),
            })],
            custom: vec![CustomToolDef {
                name: "secret".into(),
                description: Some("Returns the secret.".into()),
                input_schema: schema,
                annotations: None,
                callback: Arc::new(|_args, _meta| {
                    Box::pin(async { CustomToolOutcome::Success(json!("abc123")) })
                }),
            }],
        };
        let server = build_server(&spec);
        assert!(server.config().query.is_some());
        drop(build_mcp_service(spec));
    }
}
