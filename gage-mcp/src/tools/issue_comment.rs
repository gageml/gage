use std::future::Future;
use std::pin::Pin;

use gage_db::db::open_db;
use gage_db::issue;
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};

use crate::server::GageServer;
use crate::tool::{ToolDef, agent_author, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssueComment.md");

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new(build_tool_meta(MD), call)
}

fn call(
    _server: &GageServer,
    ctx: RequestContext<RoleServer>,
    params: JsonObject,
) -> Pin<Box<dyn Future<Output = Result<String, McpError>> + Send + '_>> {
    let author = agent_author(&ctx);
    Box::pin(handle(params, author))
}

async fn handle(params: JsonObject, author: String) -> Result<String, McpError> {
    let issue_id = req_string(&params, "issue_id")?;
    let comment = req_string(&params, "comment")?;

    let conn = open_db().unwrap();
    let issue = issue::get(&conn, &issue_id).map_err(|e| match e {
        issue::IssueError::NotFound(_) | issue::IssueError::Ambiguous(_, _) => {
            McpError::invalid_params(e.to_string(), None)
        }
        issue::IssueError::Db(_) | issue::IssueError::Duplicate(_) => {
            McpError::internal_error(e.to_string(), None)
        }
    })?;

    let now = gage_core::datetime::now_ms();
    issue::comment(&conn, &issue.id, &author, &comment, now)
        .map_err(|e| McpError::internal_error(format!("comment on issue: {e}"), None))?;

    Ok(format!(
        "Added comment to issue {} ({}).",
        gage_core::uuid::short_uuid(&issue.id),
        issue.title
    ))
}

fn req_string(params: &JsonObject, key: &str) -> Result<String, McpError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| McpError::invalid_params(format!("missing or non-string `{key}`"), None))
}
