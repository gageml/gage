use std::future::Future;
use std::pin::Pin;

use gage_scan::scanner::ScannerRegistry;
use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::ToolRoute,
    handler::server::tool::ToolCallContext,
    model::{CallToolResult, Content},
};

use crate::server::GageServer;
use crate::tool::{ToolDef, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/NoteDoc.md");

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new_dyn(
        build_tool_meta(MD),
        |ctx: ToolCallContext<'_, GageServer>| Box::pin(handle(ctx)),
    )
}

fn handle(
    ctx: ToolCallContext<'_, GageServer>,
) -> Pin<Box<dyn Future<Output = Result<CallToolResult, McpError>> + Send + '_>> {
    Box::pin(async move {
        let params = ctx.arguments.unwrap_or_default();
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::invalid_params("missing or non-string `name`", None))?;

        let registry = ScannerRegistry::load();
        match registry.note_doc(name) {
            Some(doc) => Ok(success(doc)),
            None => Ok(domain_error("Not found")),
        }
    })
}

fn success(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn domain_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}
