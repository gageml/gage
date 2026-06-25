use std::sync::Arc;

use datafusion::prelude::SessionContext;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext},
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::stdio,
};
use tokio::sync::OnceCell;

use crate::tool::ToolDef;
use crate::tools;

/// Every tool the Gage MCP server exposes. The order here is the order
/// the rmcp router registers them in.
const TOOLS: &[ToolDef] = &[
    tools::query::TOOL,
    tools::comment_write::TOOL,
    tools::issue_list::TOOL,
    tools::issue_get::TOOL,
    tools::issue_close::TOOL,
    tools::issue_comment::TOOL,
    tools::issue_open::TOOL,
    tools::note_doc::TOOL,
];

pub struct GageServer {
    tool_router: ToolRouter<Self>,
    ctx: Arc<OnceCell<SessionContext>>,
}

impl Default for GageServer {
    fn default() -> Self {
        Self::new()
    }
}

impl GageServer {
    pub fn new() -> Self {
        GageServer {
            tool_router: build_router(),
            ctx: Arc::new(OnceCell::new()),
        }
    }

    pub(crate) async fn ctx(&self) -> &SessionContext {
        self.ctx
            .get_or_init(|| async { gage_query::create_context_default().await })
            .await
    }
}

fn build_router() -> ToolRouter<GageServer> {
    let allowed = gage_tools_env();
    let mut router = ToolRouter::<GageServer>::new();
    for route in TOOLS {
        let r = route();
        if let Some(ref allow) = allowed
            && !allow.contains(r.attr.name.as_ref())
        {
            continue;
        }
        router = router.with_route(r);
    }
    router
}

/// Parse the `GAGE_TOOLS` env var: a comma-delimited list of MCP tool
/// short names (e.g. `Query,IssueOpen`). `Some(set)` restricts the
/// router to those names; `None` (unset) leaves the router unrestricted.
/// Empty or whitespace-only entries are ignored, so an empty string
/// resolves to `Some(empty_set)` — i.e. no tools exposed.
fn gage_tools_env() -> Option<std::collections::HashSet<String>> {
    let raw = std::env::var("GAGE_TOOLS").ok()?;
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

impl ServerHandler for GageServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(INSTRUCTIONS.trim().to_string()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

const INSTRUCTIONS: &str = include_str!("../config/server-instructions.md");

pub async fn serve_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let server = GageServer::new();
    server
        .serve(stdio())
        .await
        .inspect_err(|e| {
            eprintln!("gage mcp: serving error: {e:?}");
        })?
        .waiting()
        .await?;
    Ok(())
}
