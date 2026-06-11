use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_db::db::open_db;
use gage_db::issue::{self, Issue, IssueFilters, IssueStatusFilter};
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};

use crate::server::GageServer;
use crate::tool::{ToolDef, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/ListIssues.md");

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
    let status = match params.get("status").and_then(|v| v.as_str()) {
        None | Some("open") => IssueStatusFilter::Open,
        Some("closed") => IssueStatusFilter::Closed,
        Some("any") => IssueStatusFilter::Any,
        Some(other) => {
            return Err(McpError::invalid_params(
                format!("status must be 'open', 'closed', or 'any' (got '{other}')"),
                None,
            ));
        }
    };

    let conn = open_db().unwrap();
    let issues = issue::find(
        &conn,
        &IssueFilters {
            status,
            ..Default::default()
        },
    )
    .map_err(|e| McpError::internal_error(format!("list issues: {e}"), None))?;

    Ok(render_table(&issues, status))
}

fn render_table(issues: &[Issue], status: IssueStatusFilter) -> String {
    if issues.is_empty() {
        return match status {
            IssueStatusFilter::Open => "No open issues.".to_string(),
            IssueStatusFilter::Closed => "No closed issues.".to_string(),
            IssueStatusFilter::Any => "No issues.".to_string(),
        };
    }
    let mut out = String::new();
    out.push_str("| id       | name | title | status |\n");
    out.push_str("|----------|------|-------|--------|\n");
    for i in issues {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            short_uuid(&i.id),
            escape_cell(&i.name),
            escape_cell(&i.title),
            i.status.as_str(),
        ));
    }
    out
}

/// Escape `|` and newlines so a single cell stays on one row of the
/// markdown table.
fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}
