use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_db::db::open_db;
use gage_db::note::{self, Note, NoteValue};
use gage_db::scan::insert_scan_note;
use gage_db::target::{NoteTarget, ScanTarget, SessionTarget};
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};
use serde_json::Value;

use crate::server::GageServer;
use crate::tool::{ToolDef, agent_author, build_tool_meta, scan_id_from_env};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/NoteWrite.md");

const TYPES: &[&str] = &["comment", "finding"];

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
    let note_type = req_string(&params, "type")?;
    if !TYPES.contains(&note_type.as_str()) {
        return Ok(format!(
            "error: `type` must be one of {}; got `{note_type}`.",
            TYPES.join(", ")
        ));
    }
    let value = req_string(&params, "value")?;
    let session_id = opt_string(&params, "session_id");
    let session_line = opt_u32(&params, "session_line")?;
    let scan_id = scan_id_from_env();

    let target = match (session_id, session_line, &scan_id) {
        (Some(id), Some(line), _) => NoteTarget::Session(SessionTarget::new(id).with_line(line)),
        (Some(id), None, _) => NoteTarget::Session(SessionTarget::new(id)),
        (None, Some(_), _) => {
            return Ok(
                "error: `session_line` requires `session_id`; supply both or omit `session_line`."
                    .to_string(),
            );
        }
        (None, None, Some(id)) => NoteTarget::Scan(ScanTarget {
            scan_id: id.clone(),
        }),
        (None, None, None) => {
            return Ok(
                "error: no target available; provide `session_id`, or run inside an active scan."
                    .to_string(),
            );
        }
    };

    let mut note = Note::new(target, &note_type, NoteValue::from(value), &author);
    note.name = format!("{note_type}.{}", short_uuid(&note.id));

    let conn = open_db().unwrap();
    note::insert(&conn, &note)
        .map_err(|e| McpError::internal_error(format!("insert note: {e}"), None))?;

    if let Some(scan_id) = &scan_id {
        insert_scan_note(&conn, scan_id, &note.id)
            .map_err(|e| McpError::internal_error(format!("link note to scan: {e}"), None))?;
    }

    Ok(format!("Added {note_type} {}.", short_uuid(&note.id)))
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

fn opt_u32(params: &JsonObject, key: &str) -> Result<Option<u32>, McpError> {
    let Some(v) = params.get(key) else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let n = match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| {
            McpError::invalid_params(format!("`{key}` must be a positive integer"), None)
        })?,
        _ => {
            return Err(McpError::invalid_params(
                format!("`{key}` must be an integer"),
                None,
            ));
        }
    };
    u32::try_from(n)
        .map(Some)
        .map_err(|_e| McpError::invalid_params(format!("`{key}` out of range"), None))
}
