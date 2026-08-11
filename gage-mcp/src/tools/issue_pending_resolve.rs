use std::future::Future;
use std::pin::Pin;

use gage_db::db::open_db;
use gage_db::issue::IssueError;
use gage_db::resolve::{self, Resolution, ResolveError};
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};
use serde_json::Value;

use crate::server::GageServer;
use crate::tool::{ToolDef, agent_author, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssuePendingResolve.md");

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
    let resolutions = parse_resolutions(&params)?;

    let conn = open_db().unwrap();
    let now = gage_core::datetime::now_ms();
    let applied = resolve::apply(&conn, &resolutions, &author, now).map_err(|e| match e {
        ResolveError::Invalid(_) => McpError::invalid_params(e.to_string(), None),
        ResolveError::Issue(IssueError::NotFound(_) | IssueError::Ambiguous(_, _)) => {
            McpError::invalid_params(e.to_string(), None)
        }
        ResolveError::Issue(_) => McpError::internal_error(e.to_string(), None),
    })?;

    let mut parts = vec![format!("{} promoted", applied.promoted)];
    let mut closed = format!("{} closed as duplicate", applied.closed);
    if applied.reopened > 0 {
        closed.push_str(&format!(" ({} target(s) reopened)", applied.reopened));
    }
    parts.push(closed);
    if applied.comments > 0 {
        parts.push(format!("{} comment(s) added", applied.comments));
    }
    Ok(format!(
        "Applied {} resolution(s): {}. {} pending issue(s) remain.",
        resolutions.len(),
        parts.join(", "),
        applied.pending_remaining
    ))
}

fn parse_resolutions(params: &JsonObject) -> Result<Vec<Resolution>, McpError> {
    let items = params
        .get("resolutions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError::invalid_params("missing or non-array `resolutions`", None))?;
    if items.is_empty() {
        return Err(McpError::invalid_params("`resolutions` is empty", None));
    }
    items.iter().map(parse_resolution).collect()
}

fn parse_resolution(item: &Value) -> Result<Resolution, McpError> {
    let obj = item
        .as_object()
        .ok_or_else(|| McpError::invalid_params("each resolution must be an object", None))?;
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "issue" | "action" | "of" | "comment" | "reopen"
        ) {
            return Err(McpError::invalid_params(
                format!("unknown resolution field `{key}`"),
                None,
            ));
        }
    }
    let issue = req_str(obj, "issue")?;
    let action = req_str(obj, "action")?;
    let of = opt_str(obj, "of")?;
    let comment = opt_str(obj, "comment")?;
    let reopen = match obj.get("reopen") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(McpError::invalid_params("`reopen` must be a boolean", None));
        }
    };

    match action.as_str() {
        "open" => {
            if of.is_some() || comment.is_some() || reopen {
                return Err(McpError::invalid_params(
                    format!(
                        "action `open` for issue {issue} takes no `of`, `comment`, or `reopen`"
                    ),
                    None,
                ));
            }
            Ok(Resolution::Open { issue })
        }
        "duplicate" => {
            let of = of.ok_or_else(|| {
                McpError::invalid_params(
                    format!("action `duplicate` for issue {issue} requires `of`"),
                    None,
                )
            })?;
            Ok(Resolution::Duplicate {
                issue,
                of,
                comment,
                reopen,
            })
        }
        other => Err(McpError::invalid_params(
            format!("action must be `open` or `duplicate` (got `{other}`)"),
            None,
        )),
    }
}

fn req_str(obj: &JsonObject, key: &str) -> Result<String, McpError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            McpError::invalid_params(format!("missing or non-string `{key}` in resolution"), None)
        })
}

fn opt_str(obj: &JsonObject, key: &str) -> Result<Option<String>, McpError> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(McpError::invalid_params(
            format!("`{key}` must be a non-empty string"),
            None,
        )),
    }
}
