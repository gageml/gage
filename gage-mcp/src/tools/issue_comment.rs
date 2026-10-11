//! `IssueComment`: add a comment to an issue in the store.

use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_store::IssueStore;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};

use crate::server::GageServer;
use crate::service::StoreWriterConfig;
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};
use crate::tools::issue_update::required_string;

pub const NAME: &str = "IssueComment";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssueComment.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "IssueComment description exceeds Claude Code's 2048-byte cap",
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
        let Some(config) = &ctx.service.config().issue_comment else {
            return Err(McpError::internal_error(
                "IssueComment tool is routed but not configured",
                None,
            ));
        };
        execute(config, params)
    })
}

/// Parse the call and write the comment. A call that violates the
/// declared parameters is an invalid-params error; an unknown issue is
/// a result the model reads.
pub fn execute(config: &StoreWriterConfig, params: JsonObject) -> Result<CallToolResult, McpError> {
    let issue_id = required_string(&params, "issue_id")?;
    let comment = required_string(&params, "comment")?;
    let store = config.store();
    let issues = IssueStore::from(&*store);
    let issue = match issues.get(&issue_id) {
        Ok(issue) => issue,
        Err(e) => return Ok(domain_error(e.to_string())),
    };
    match issues.comment(&issue.id, &config.author, &comment) {
        Ok(_) => Ok(success(format!(
            "Commented on issue {} ({}).",
            short_uuid(&issue.id),
            issue.title
        ))),
        Err(e) => Ok(domain_error(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use gage_store::IssueStatus;
    use serde_json::json;

    use super::*;
    use crate::tools::store_test::{config, issue, object, text_of};

    #[test]
    fn records_a_comment_event_with_the_configured_author() {
        let (_tmp, config) = config();
        let id = issue(&config, "Something", IssueStatus::Open);
        let r = execute(&config, object(json!({"issue_id": id, "comment": "noted"}))).unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        let store = config.store.lock().unwrap();
        let full = IssueStore::from(&*store).get(&id).unwrap();
        assert_eq!(full.changes[1].message.as_deref(), Some("noted"));
        assert_eq!(full.changes[1].author, "user:t");
    }

    #[test]
    fn a_missing_comment_is_invalid_params_and_an_unknown_issue_a_refusal() {
        let (_tmp, config) = config();
        let id = issue(&config, "Something", IssueStatus::Open);
        assert!(execute(&config, object(json!({"issue_id": id, "comment": "  "}))).is_err());
        let r = execute(&config, object(json!({"issue_id": "nope", "comment": "x"}))).unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
