use std::future::Future;
use std::pin::Pin;

use gage_db::db::open_db;
use gage_db::issue::{self, IssueStatus, StatusReason};
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};

use crate::server::GageServer;
use crate::tool::{ToolDef, agent_author, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssueUpdate.md");

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new(build_tool_meta(MD), call)
}

fn call(
    server: &GageServer,
    ctx: RequestContext<RoleServer>,
    params: JsonObject,
) -> Pin<Box<dyn Future<Output = Result<String, McpError>> + Send + '_>> {
    let author = agent_author(server, &ctx);
    Box::pin(handle(params, author))
}

async fn handle(params: JsonObject, author: String) -> Result<String, McpError> {
    let issue_id = req_string(&params, "issue_id")?;
    let status = opt_enum::<IssueStatus>(&params, "status")?;
    let reason = opt_enum::<StatusReason>(&params, "status_reason")?;
    let message = opt_string(&params, "message");

    let conn = open_db().unwrap();
    let issue = issue::get(&conn, &issue_id).map_err(|e| match e {
        issue::IssueError::NotFound(_) | issue::IssueError::Ambiguous(_, _) => {
            McpError::invalid_params(e.to_string(), None)
        }
        issue::IssueError::Db(_) | issue::IssueError::InvalidSessionId(_) => {
            McpError::internal_error(e.to_string(), None)
        }
    })?;

    let Some(new_status) = status else {
        return Ok(format!(
            "No status supplied for issue {} ({}); nothing to update.",
            gage_core::uuid::short_uuid(&issue.id),
            issue.title
        ));
    };

    issue::set_status(
        &conn,
        &issue.id,
        new_status,
        reason,
        &author,
        message.as_deref(),
    )
    .map_err(|e| McpError::internal_error(format!("update issue: {e}"), None))?;

    let suffix = match (new_status, reason) {
        (IssueStatus::Closed, Some(r)) => format!(" ({})", r.as_str()),
        _ => String::new(),
    };
    Ok(format!(
        "Set issue {} ({}) to {}{}.",
        gage_core::uuid::short_uuid(&issue.id),
        issue.title,
        new_status.as_str(),
        suffix,
    ))
}

fn req_string(params: &JsonObject, key: &str) -> Result<String, McpError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| McpError::invalid_params(format!("missing or non-string `{key}`"), None))
}

fn opt_string(params: &JsonObject, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn opt_enum<T: std::str::FromStr<Err = String>>(
    params: &JsonObject,
    key: &str,
) -> Result<Option<T>, McpError> {
    let Some(v) = params.get(key) else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let s = v
        .as_str()
        .ok_or_else(|| McpError::invalid_params(format!("`{key}` must be a string"), None))?;
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<T>()
        .map(Some)
        .map_err(|e| McpError::invalid_params(format!("`{key}`: {e}"), None))
}
