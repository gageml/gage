//! Build an HTTP MCP service from a [`ToolSpec`].
//!
//! The spec selects which built-in Gage tools to expose and optionally
//! adds [`CustomToolDef`]s — externally-supplied tool definitions whose
//! bodies are opaque async closures. Both kinds of tools surface as
//! ordinary MCP tools to the client; the gage-mcp crate stays unaware
//! of where custom-tool bodies come from.
//!
//! [`build_mcp_service`] returns the [`crate::host::RegisteredService`]
//! shape an [`crate::host::McpHost`] expects.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, Content, JsonObject, Tool as ToolMeta};
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use tower::service_fn;
use tower::util::BoxCloneSyncService;

use crate::host::RegisteredService;
use crate::server::{GageServer, TOOLS};

/// What an MCP service exposes: a subset of the built-in Gage tools
/// plus zero or more externally-supplied tool definitions.
#[derive(Default)]
pub struct ToolSpec {
    /// Built-in Gage tool names to include (e.g. `"Query"`,
    /// `"IssueWrite"`). Names not recognized by the built-in router are
    /// silently ignored — the caller is the source of truth for which
    /// names exist.
    pub gage_tools: Vec<String>,
    pub custom_tools: Vec<CustomToolDef>,
}

/// One externally-supplied MCP tool: the wire-visible metadata plus a
/// callback that runs when the model invokes it.
pub struct CustomToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input. The object form expected by
    /// MCP — i.e. `{"type": "object", "properties": {...}, "required":
    /// [...]}`. Pass an empty object schema for tools that take no
    /// arguments.
    pub input_schema: JsonObject,
    pub callback: CustomToolCallback,
}

/// Async closure invoked when the model calls a [`CustomToolDef`].
/// Receives the tool's argument object as a JSON value; returns either
/// a success value (rendered as JSON text in the tool result) or an
/// error string (returned to the model as a tool error).
pub type CustomToolCallback =
    Arc<dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> + Send + Sync>;

/// Construct a streamable-HTTP MCP service exposing every tool the
/// spec declares. The returned service plugs into an
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
    let allowed: HashSet<&str> = spec.gage_tools.iter().map(String::as_str).collect();
    let mut router = rmcp::handler::server::router::tool::ToolRouter::<GageServer>::new();
    for route in TOOLS {
        let r = route();
        if !allowed.contains(r.attr.name.as_ref()) {
            continue;
        }
        router = router.with_route(r);
    }
    for def in &spec.custom_tools {
        router = router.with_route(custom_route(def));
    }
    GageServer::with_router(router)
}

fn custom_route(def: &CustomToolDef) -> ToolRoute<GageServer> {
    let meta = ToolMeta {
        name: def.name.clone().into(),
        title: None,
        description: Some(def.description.clone().into()),
        input_schema: Arc::new(def.input_schema.clone()),
        output_schema: None,
        annotations: None,
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
        let callback = Arc::clone(&callback);
        Box::pin(async move {
            match (callback)(args).await {
                Ok(out) => Ok(CallToolResult::success(vec![Content::text(render_output(
                    &out,
                ))])),
                Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
            }
        })
    })
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

    fn empty_object_schema() -> JsonObject {
        let mut o = JsonObject::new();
        o.insert("type".into(), Value::String("object".into()));
        o.insert("properties".into(), Value::Object(JsonObject::new()));
        o.insert("required".into(), Value::Array(vec![]));
        o
    }

    #[test]
    fn builds_with_no_tools() {
        let svc = build_mcp_service(ToolSpec::default());
        // Constructed without panic; can't easily exercise without a
        // wire client.
        drop(svc);
    }

    #[test]
    fn builds_with_gage_and_custom_tools() {
        let spec = ToolSpec {
            gage_tools: vec!["Query".into(), "IssueWrite".into()],
            custom_tools: vec![CustomToolDef {
                name: "secret".into(),
                description: "Returns the secret.".into(),
                input_schema: empty_object_schema(),
                callback: Arc::new(|_args| Box::pin(async { Ok(json!("abc123")) })),
            }],
        };
        let svc = build_mcp_service(spec);
        drop(svc);
    }

    // End-to-end wire test deferred to step 6.4: speaking the
    // streamable-HTTP MCP protocol by hand (initialize → session id →
    // tools/list) without an MCP client is its own non-trivial work
    // and will fall out for free once `call_agent` drives the path.
}
