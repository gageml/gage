//! The rmcp server handler behind one service: a router of the
//! service's tools and the configuration the Gage tools read.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};

use crate::service::ToolsConfig;
use crate::tool::MAX_DESCRIPTION_BYTES;

pub struct GageServer {
    tool_router: ToolRouter<Self>,
    config: ToolsConfig,
}

impl GageServer {
    pub fn new(tool_router: ToolRouter<Self>, config: ToolsConfig) -> Self {
        GageServer {
            tool_router,
            config,
        }
    }

    pub(crate) fn config(&self) -> &ToolsConfig {
        &self.config
    }
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
const _: () = assert!(
    INSTRUCTIONS.len() <= MAX_DESCRIPTION_BYTES,
    "server instructions exceed Claude Code's 4096-byte cap",
);

#[cfg(test)]
mod tests {
    use super::INSTRUCTIONS;
    use crate::tools::{
        issue_assign, issue_comment, issue_pending_resolve, issue_update, issue_write, issues_free,
        note_write, query,
    };

    #[test]
    fn instructions_name_every_tool() {
        let names = [
            query::NAME,
            issue_write::NAME,
            note_write::NAME,
            issue_update::NAME,
            issue_assign::NAME,
            issues_free::NAME,
            issue_comment::NAME,
            issue_pending_resolve::NAME,
        ];
        for name in names {
            assert!(
                INSTRUCTIONS.contains(&format!("`{name}`")),
                "server instructions do not name {name}"
            );
        }
    }
}
