use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_db::db::open_db;
use gage_db::issue::{self, Issue, IssueEvidence};
use gage_db::note;
use gage_db::scan::insert_scan_issue;
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};

use crate::server::GageServer;
use crate::tool::{ToolDef, agent_author, build_tool_meta, scan_id_from_env};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssueOpen.md");

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
    let title = req_string(&params, "title")?;
    let description = opt_string(&params, "description");
    let target = opt_string(&params, "target").unwrap_or_default();
    let evidence_ids = opt_string_array(&params, "evidence")?;

    let conn = open_db().unwrap();

    // Resolve any supplied note IDs up front so a bad reference fails
    // before we insert the issue, and we capture each note's name for
    // the evidence row.
    let mut evidence_notes = Vec::with_capacity(evidence_ids.len());
    for id in &evidence_ids {
        let n = note::get(&conn, id).map_err(|e| match e {
            note::NoteError::NotFound(_) | note::NoteError::Ambiguous(_, _) => {
                McpError::invalid_params(format!("evidence note `{id}`: {e}"), None)
            }
            _ => McpError::internal_error(format!("lookup evidence note `{id}`: {e}"), None),
        })?;
        evidence_notes.push(n);
    }

    let issue_row = Issue::new(target, "issue.", title, description, &author);
    let now = issue_row.created;

    issue::insert(&conn, &issue_row).map_err(|e| match e {
        issue::IssueError::Duplicate(prev) => McpError::internal_error(
            format!(
                "issue id collision against existing issue {} ({}); \
                 inspect the judge sandbox db and report",
                short_uuid(&prev.id),
                prev.title
            ),
            None,
        ),
        _ => McpError::internal_error(format!("insert issue: {e}"), None),
    })?;

    if let Some(scan_id) = scan_id_from_env() {
        insert_scan_issue(&conn, &scan_id, &issue_row.id)
            .map_err(|e| McpError::internal_error(format!("link issue to scan: {e}"), None))?;
    }

    for n in &evidence_notes {
        issue::insert_issue_evidence(
            &conn,
            &IssueEvidence {
                issue_id: issue_row.id.clone(),
                note_id: n.id.clone(),
                name: n.name.clone(),
                timestamp: now,
                digest: None,
            },
        )
        .map_err(|e| McpError::internal_error(format!("link evidence: {e}"), None))?;
    }

    Ok(format!(
        "Opened issue {} ({}) with {} evidence note(s).",
        short_uuid(&issue_row.id),
        issue_row.title,
        evidence_notes.len()
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

fn opt_string_array(params: &JsonObject, key: &str) -> Result<Vec<String>, McpError> {
    let Some(v) = params.get(key) else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v
        .as_array()
        .ok_or_else(|| McpError::invalid_params(format!("`{key}` must be an array"), None))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let s = item.as_str().ok_or_else(|| {
            McpError::invalid_params(format!("`{key}[{i}]` must be a string"), None)
        })?;
        out.push(s.to_string());
    }
    Ok(out)
}
