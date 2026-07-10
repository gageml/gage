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
pub(crate) const TOOLS: &[ToolDef] = &[
    tools::query::TOOL,
    tools::issue_close::TOOL,
    tools::issue_comment::TOOL,
    tools::issue_write::TOOL,
    tools::note_write::TOOL,
];

pub struct GageServer {
    tool_router: ToolRouter<Self>,
    ctx: Arc<OnceCell<SessionContext>>,
    /// Author base for writes made through this server's tools (e.g.
    /// `agent:{scanner}`); each request appends its own
    /// `?call={toolUseId}`. Set when the server fronts a single agent
    /// invocation; `None` for ad-hoc servers (e.g. the stdio server),
    /// where the base is derived from the client's initialize info.
    agent_author: Option<String>,
}

impl Default for GageServer {
    fn default() -> Self {
        Self::new()
    }
}

impl GageServer {
    pub fn new() -> Self {
        GageServer::with_router(build_router())
    }

    /// Construct a server backed by a custom router. Used by
    /// [`crate::service::build_mcp_service`] when the tool set is
    /// driven by a [`crate::service::ToolSpec`].
    pub fn with_router(tool_router: ToolRouter<Self>) -> Self {
        GageServer {
            tool_router,
            ctx: Arc::new(OnceCell::new()),
            agent_author: None,
        }
    }

    /// Set the agent-instance author for writes made through this
    /// server (see the `agent_author` field).
    pub fn with_author(mut self, author: Option<String>) -> Self {
        self.agent_author = author;
        self
    }

    pub(crate) fn agent_author(&self) -> Option<&str> {
        self.agent_author.as_deref()
    }

    /// The DataFusion context that the `Query` MCP tool runs against.
    /// When the calling process has a `GAGE_SCAN_ID` in its
    /// environment (set by `gage scan` and `gage agent` for the
    /// duration of a run), the context is scoped to that scan via
    /// [`gage_query::create_agent_context`]; reads return only rows
    /// linked through `scan_session` / `scan_note` / `scan_issue`.
    /// Absent the env var (e.g. ad-hoc MCP clients), the default
    /// unscoped context is built.
    pub(crate) async fn ctx(&self) -> &SessionContext {
        self.ctx
            .get_or_init(|| async {
                match crate::tool::scan_id_from_env() {
                    Some(scan_id) => gage_query::create_agent_context(scan_id).await,
                    None => gage_query::create_context_default().await,
                }
            })
            .await
    }
}

fn build_router() -> ToolRouter<GageServer> {
    let mut router = ToolRouter::<GageServer>::new();
    for route in TOOLS {
        router = router.with_route(route());
    }
    router
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
