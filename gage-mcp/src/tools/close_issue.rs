use std::future::Future;
use std::pin::Pin;

use gage_db::db::open_db;
use gage_db::issue::{self, ClosedReason, IssueFilters, IssueStatusFilter};
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};

use crate::server::GageServer;
use crate::tool::{ToolDef, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/CloseIssue.md");

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new(build_tool_meta(MD), call)
}

fn call(
    _server: &GageServer,
    _ctx: RequestContext<RoleServer>,
    params: JsonObject,
) -> Pin<Box<dyn Future<Output = Result<String, McpError>> + Send + '_>> {
    Box::pin(handle(params))
}

async fn handle(params: JsonObject) -> Result<String, McpError> {
    let issue_id = req_string(&params, "issue_id")?;
    let reason_str = req_string(&params, "reason")?;
    let message = opt_string(&params, "message");

    let reason = match reason_str.as_str() {
        "completed" => ClosedReason::Completed,
        "skipped" => ClosedReason::Skipped,
        other => {
            return Err(McpError::invalid_params(
                format!("reason must be 'completed' or 'skipped' (got '{other}')"),
                None,
            ));
        }
    };

    let conn = open_db();
    let issue = issue::get(&conn, &issue_id).map_err(|e| match e {
        issue::IssueError::NotFound(_) | issue::IssueError::Ambiguous(_, _) => {
            McpError::invalid_params(e.to_string(), None)
        }
        issue::IssueError::Db(_) | issue::IssueError::Duplicate(_) => {
            McpError::internal_error(e.to_string(), None)
        }
    })?;

    let now = gage_core::datetime::now_ms();
    let author = format!(
        "user:{}",
        std::env::var("USER").unwrap_or_else(|_| "unknown".into())
    );
    issue::close(&conn, &issue.id, reason, &author, message.as_deref(), now)
        .map_err(|e| McpError::internal_error(format!("close issue: {e}"), None))?;

    let open = issue::find(
        &conn,
        &IssueFilters {
            status: IssueStatusFilter::Open,
            ..Default::default()
        },
    )
    .map_err(|e| McpError::internal_error(format!("count open issues: {e}"), None))?;
    let remaining = match open.len() {
        0 => "No open issues remain.".to_string(),
        1 => "1 open issue remains.".to_string(),
        n => format!("{n} open issues remain."),
    };
    Ok(format!(
        "Closed issue {} ({}). {remaining}",
        gage_core::uuid::short_uuid(&issue.id),
        reason.as_str()
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
