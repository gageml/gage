//! `IssuesFree`: reopen assigned issues whose session is not live.
//!
//! The tool reads every assigned issue, asks the session source
//! whether the holding session is still a live instance, and reopens
//! the issues whose holder is gone. A holder that is still live keeps
//! its issue; a holder whose liveness cannot be determined is left
//! assigned and named in the result, so an inconclusive check frees
//! nothing.

use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_store::{IssueFull, IssueStatus, IssueStore};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::CallToolResult;

use crate::server::GageServer;
use crate::service::IssuesFreeConfig;
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};

pub const NAME: &str = "IssuesFree";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssuesFree.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "IssuesFree description exceeds Claude Code's description cap",
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
        let Some(config) = &ctx.service.config().issues_free else {
            return Err(McpError::internal_error(
                "IssuesFree tool is routed but not configured",
                None,
            ));
        };
        Ok(execute(config))
    })
}

/// Reopen every assigned issue whose holding session is not live.
pub fn execute(config: &IssuesFreeConfig) -> CallToolResult {
    let store = config.writer.store();
    let issues = IssueStore::from(&*store);
    let author = &config.writer.author;

    let assigned = match assigned_issues(&issues) {
        Ok(assigned) => assigned,
        Err(e) => return domain_error(e),
    };

    let mut freed: Vec<String> = Vec::new();
    let mut live = 0usize;
    let mut left: Vec<String> = Vec::new();

    for issue in &assigned {
        let short = short_uuid(&issue.id);
        let Some(session) = holder(issue) else {
            left.push(format!("{short}: no session recorded on the assignment"));
            continue;
        };
        match (config.liveness)(session) {
            Ok(true) => live += 1,
            Ok(false) => {
                let message = format!("freed: session {session} is no longer live");
                match issues.set_status(&issue.id, IssueStatus::Open, None, author, Some(&message))
                {
                    Ok(_) => freed.push(format!("{short} {}", issue.title)),
                    Err(e) => left.push(format!("{short}: reopen failed: {e}")),
                }
            }
            Err(reason) => left.push(format!("{short}: {reason}")),
        }
    }

    success(report(&freed, live, &left))
}

/// Every live issue whose status is `assigned`.
fn assigned_issues(issues: &IssueStore<'_>) -> Result<Vec<IssueFull>, String> {
    let mut assigned = Vec::new();
    for issue in issues.iter().map_err(|e| e.to_string())? {
        let issue = issue.map_err(|e| e.to_string())?;
        if issue.status == IssueStatus::Assigned {
            assigned.push(issue);
        }
    }
    Ok(assigned)
}

/// The session id recorded on the issue's most recent assignment: the
/// message of the latest change that set status to `assigned`.
fn holder(issue: &IssueFull) -> Option<&str> {
    issue
        .changes
        .iter()
        .rev()
        .find(|change| change.to_status == Some(IssueStatus::Assigned))
        .and_then(|change| change.message.as_deref())
}

fn report(freed: &[String], live: usize, left: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    match freed.len() {
        0 => lines.push("Freed no issues.".to_string()),
        n => {
            lines.push(format!("Freed {n}:"));
            lines.extend(freed.iter().map(|f| format!("- {f}")));
        }
    }
    if live > 0 {
        lines.push(format!("Held by a live session: {live}."));
    }
    if !left.is_empty() {
        lines.push(format!(
            "Left assigned, liveness undetermined: {}.",
            left.len()
        ));
        lines.extend(left.iter().map(|u| format!("- {u}")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gage_store::IssueStore;

    use super::*;
    use crate::service::{IssuesFreeConfig, SessionLivenessFn, StoreWriterConfig};
    use crate::tools::store_test::{config, issue, text_of};

    fn status(writer: &StoreWriterConfig, id: &str) -> IssueStatus {
        let store = writer.store.lock().unwrap();
        IssueStore::from(&*store).get(id).unwrap().status
    }

    #[test]
    fn frees_dead_holders_keeps_live_and_leaves_undetermined() {
        let (_tmp, writer) = config();
        let open = issue(&writer, "Plain open", IssueStatus::Open);
        let dead = issue(&writer, "Dead holder", IssueStatus::Open);
        let alive = issue(&writer, "Live holder", IssueStatus::Open);
        let murky = issue(&writer, "Unknown holder", IssueStatus::Open);
        {
            let store = writer.store.lock().unwrap();
            let s = IssueStore::from(&*store);
            s.assign(&dead, "user:t", "dead-session", false).unwrap();
            s.assign(&alive, "user:t", "live-session", false).unwrap();
            s.assign(&murky, "user:t", "murky-session", false).unwrap();
        }

        let liveness: SessionLivenessFn = Arc::new(|id: &str| match id {
            "live-session" => Ok(true),
            "dead-session" => Ok(false),
            other => Err(format!("cannot probe {other}")),
        });
        let cfg = IssuesFreeConfig {
            writer: writer.clone(),
            liveness,
        };

        let result = execute(&cfg);
        assert_eq!(result.is_error, Some(false), "{}", text_of(&result));
        let text = text_of(&result);
        assert!(text.contains("Dead holder"), "{text}");

        assert_eq!(status(&writer, &dead), IssueStatus::Open);
        assert_eq!(status(&writer, &alive), IssueStatus::Assigned);
        assert_eq!(status(&writer, &murky), IssueStatus::Assigned);
        assert_eq!(status(&writer, &open), IssueStatus::Open);
    }

    #[test]
    fn reopened_issue_records_the_dead_session() {
        let (_tmp, writer) = config();
        let dead = issue(&writer, "Dead holder", IssueStatus::Open);
        {
            let store = writer.store.lock().unwrap();
            IssueStore::from(&*store)
                .assign(&dead, "user:t", "dead-session", false)
                .unwrap();
        }
        let liveness: SessionLivenessFn = Arc::new(|_id: &str| Ok(false));
        let cfg = IssuesFreeConfig {
            writer: writer.clone(),
            liveness,
        };

        execute(&cfg);

        let store = writer.store.lock().unwrap();
        let full = IssueStore::from(&*store).get(&dead).unwrap();
        let last = full.changes.last().unwrap();
        assert_eq!(last.to_status, Some(IssueStatus::Open));
        assert!(
            last.message.as_deref().unwrap().contains("dead-session"),
            "{:?}",
            last.message
        );
    }

    #[test]
    fn no_assigned_issues_frees_nothing() {
        let (_tmp, writer) = config();
        issue(&writer, "Plain open", IssueStatus::Open);
        let liveness: SessionLivenessFn = Arc::new(|_id: &str| Ok(false));
        let cfg = IssuesFreeConfig {
            writer: writer.clone(),
            liveness,
        };
        let result = execute(&cfg);
        assert_eq!(result.is_error, Some(false), "{}", text_of(&result));
        assert!(
            text_of(&result).contains("Freed no issues"),
            "{}",
            text_of(&result)
        );
    }
}
