//! `IssueUpdate`: open or close an issue in the store.

use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_store::{IssueStatus, IssueStore, StatusReason};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};

use crate::server::GageServer;
use crate::service::StoreWriterConfig;
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};

pub const NAME: &str = "IssueUpdate";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssueUpdate.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "IssueUpdate description exceeds Claude Code's 4096-byte cap",
);

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new_dyn(
        build_tool_meta(MD),
        |ctx: ToolCallContext<'_, GageServer>| Box::pin(handle(ctx)),
    )
}

fn handle(
    ctx: ToolCallContext<'_, GageServer>,
) -> Pin<Box<dyn Future<Output = Result<CallToolResult, McpError>> + Send + '_>> {
    Box::pin(async move {
        let params = ctx.arguments.unwrap_or_default();
        let Some(config) = &ctx.service.config().issue_update else {
            return Err(McpError::internal_error(
                "IssueUpdate tool is routed but not configured",
                None,
            ));
        };
        execute(config, params)
    })
}

/// What one call asks for, parsed
#[derive(Debug, PartialEq, Eq)]
enum Update {
    Open,
    Close(StatusReason),
    /// Close as a duplicate of the surviving issue
    Duplicate {
        duplicate_of: String,
    },
}

/// Parse the call and apply it. A call that violates the declared
/// parameters is an invalid-params error; a store refusal, such as an
/// unknown id or a status the issue already has, is a result the
/// model reads.
pub fn execute(config: &StoreWriterConfig, params: JsonObject) -> Result<CallToolResult, McpError> {
    let issue_id = required_string(&params, "issue_id")?;
    let update = parse_update(&params)?;
    let message = optional_string(&params, "message")?;
    let store = config.store();
    let issues = IssueStore::from(&*store);
    let issue = match issues.get(&issue_id) {
        Ok(issue) => issue,
        Err(e) => return Ok(domain_error(e.to_string())),
    };
    let author = &config.author;
    // A completed or wontfix close with no message leaves the history
    // with a status change and no record of why; the duplicate close
    // carries its reason as the survivor link
    if message.is_none()
        && let Update::Close(reason) = &update
    {
        return Ok(domain_error(format!(
            "closing as {reason} requires `message`: state what was done or why the issue \
             will not be addressed"
        )));
    }
    let message = message.as_deref();
    let (result, outcome) = match &update {
        Update::Open => (
            issues.set_status(&issue.id, IssueStatus::Open, None, author, message),
            "open".to_string(),
        ),
        Update::Close(reason) => (
            issues.set_status(
                &issue.id,
                IssueStatus::Closed,
                Some(*reason),
                author,
                message,
            ),
            format!("closed ({reason})"),
        ),
        Update::Duplicate { duplicate_of } => match issues.get(duplicate_of) {
            Ok(survivor) => (
                issues.close_duplicate(&issue.id, &survivor.id, author, message),
                format!("closed (duplicate of {})", short_uuid(&survivor.id)),
            ),
            Err(e) => return Ok(domain_error(format!("duplicate_of: {e}"))),
        },
    };
    match result {
        Ok(_) => Ok(success(format!(
            "Set issue {} ({}) to {outcome}.",
            short_uuid(&issue.id),
            issue.title
        ))),
        Err(e) => Ok(domain_error(e.to_string())),
    }
}

fn parse_update(params: &JsonObject) -> Result<Update, McpError> {
    let status: IssueStatus = required_string(params, "status")?.parse().map_err(|e| {
        McpError::invalid_params(format!("`status` must be open or closed: {e}"), None)
    })?;
    let reason = match optional_string(params, "status_reason")? {
        Some(s) => Some(s.parse::<StatusReason>().map_err(|e| {
            McpError::invalid_params(
                format!("`status_reason` must be completed, wontfix, or duplicate: {e}"),
                None,
            )
        })?),
        None => None,
    };
    let duplicate_of = optional_string(params, "duplicate_of")?;
    match (status, reason, duplicate_of) {
        (IssueStatus::Open, None, None) => Ok(Update::Open),
        (IssueStatus::Open, Some(_), _) => Err(McpError::invalid_params(
            "`status_reason` applies to closing only",
            None,
        )),
        (IssueStatus::Closed, Some(StatusReason::Duplicate), Some(duplicate_of)) => {
            Ok(Update::Duplicate { duplicate_of })
        }
        (IssueStatus::Closed, Some(StatusReason::Duplicate), None) => {
            Err(McpError::invalid_params(
                "a duplicate close names the surviving issue in `duplicate_of`",
                None,
            ))
        }
        (_, _, Some(_)) => Err(McpError::invalid_params(
            "`duplicate_of` applies to a duplicate close only",
            None,
        )),
        (IssueStatus::Closed, reason, None) => {
            Ok(Update::Close(reason.unwrap_or(StatusReason::Completed)))
        }
        (IssueStatus::Assigned, _, _) => Err(McpError::invalid_params(
            "`status` must be open or closed; an issue is assigned through IssueAssign",
            None,
        )),
        (IssueStatus::Pending | IssueStatus::Unknown, _, _) => Err(McpError::invalid_params(
            "`status` must be open or closed",
            None,
        )),
    }
}

pub(crate) fn required_string(params: &JsonObject, key: &str) -> Result<String, McpError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| McpError::invalid_params(format!("missing or empty `{key}`"), None))
}

/// An optional string parameter; absent, null, and blank are `None`
pub(crate) fn optional_string(params: &JsonObject, key: &str) -> Result<Option<String>, McpError> {
    match params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(McpError::invalid_params(
            format!("`{key}` must be a string"),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::store_test::{config, issue, object, text_of};

    #[test]
    fn open_releases_an_assigned_issue_and_assigned_is_not_a_status_input() {
        let (_tmp, config) = config();
        let id = issue(&config, "Open", IssueStatus::Open);
        {
            let store = config.store.lock().unwrap();
            IssueStore::from(&*store)
                .assign(&id, "user:t", "session a", false)
                .unwrap();
        }

        let r = execute(&config, object(json!({"issue_id": id, "status": "open"}))).unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        {
            let store = config.store.lock().unwrap();
            let full = IssueStore::from(&*store).get(&id).unwrap();
            assert_eq!(full.status, IssueStatus::Open);
        }

        let err = execute(
            &config,
            object(json!({"issue_id": id, "status": "assigned"})),
        )
        .unwrap_err();
        assert!(err.message.contains("IssueAssign"), "{}", err.message);
    }

    #[test]
    fn a_completed_or_wontfix_close_requires_a_message() {
        let (_tmp, config) = config();
        let id = issue(&config, "Open", IssueStatus::Open);
        for params in [
            json!({"issue_id": id, "status": "closed"}),
            json!({"issue_id": id, "status": "closed", "status_reason": "wontfix", "message": "  "}),
        ] {
            let r = execute(&config, object(params)).unwrap();
            assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
            assert!(
                text_of(&r).contains("requires `message`"),
                "{}",
                text_of(&r)
            );
        }
        {
            let store = config.store.lock().unwrap();
            let full = IssueStore::from(&*store).get(&id).unwrap();
            assert_eq!(full.status, IssueStatus::Open);
            assert_eq!(full.changes.len(), 1);
        }
        let r = execute(
            &config,
            object(json!({"issue_id": id, "status": "closed", "message": "fixed in abc123"})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
    }

    #[test]
    fn opens_closes_and_records_a_duplicate_survivor() {
        let (_tmp, config) = config();
        let pending = issue(&config, "Retry loop spins", IssueStatus::Pending);
        let survivor = issue(&config, "Retry loop never ends", IssueStatus::Open);

        let r = execute(
            &config,
            object(
                json!({"issue_id": pending, "status": "closed", "status_reason": "duplicate",
                          "duplicate_of": survivor, "message": "same condition"}),
            ),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        assert!(text_of(&r).contains("duplicate of"), "{}", text_of(&r));
        {
            let store = config.store.lock().unwrap();
            let full = IssueStore::from(&*store).get(&pending).unwrap();
            assert_eq!(full.status_reason, Some(StatusReason::Duplicate));
            assert_eq!(
                full.changes[1].duplicate_of.as_deref(),
                Some(survivor.as_str())
            );
            assert_eq!(full.changes[1].author, "user:t");
        }

        let r = execute(
            &config,
            object(
                json!({"issue_id": survivor, "status": "closed", "status_reason": "wontfix",
                          "message": "out of scope"}),
            ),
        )
        .unwrap();
        assert_eq!(
            text_of(&r),
            format!(
                "Set issue {} (Retry loop never ends) to closed (wontfix).",
                short_uuid(&survivor)
            )
        );
        let r = execute(
            &config,
            object(json!({"issue_id": survivor, "status": "open"})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false));
        // Already open: a refusal the model reads
        let r = execute(
            &config,
            object(json!({"issue_id": survivor, "status": "open"})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[test]
    fn malformed_calls_are_invalid_params() {
        let (_tmp, config) = config();
        let id = issue(&config, "Something", IssueStatus::Open);
        for params in [
            json!({"status": "closed"}),
            json!({"issue_id": id, "status": "pending"}),
            json!({"issue_id": id, "status": "open", "status_reason": "completed"}),
            json!({"issue_id": id, "status": "closed", "status_reason": "duplicate"}),
            json!({"issue_id": id, "status": "closed", "status_reason": "completed", "duplicate_of": "x"}),
        ] {
            assert!(
                execute(&config, object(params.clone())).is_err(),
                "{params}"
            );
        }
        let r = execute(
            &config,
            object(json!({"issue_id": "nope", "status": "open"})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
