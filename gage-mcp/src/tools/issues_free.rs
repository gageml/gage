//! `IssuesFree`: reopen assigned issues whose session is gone.
//!
//! The tool reads every assigned issue and asks whether the session
//! holding it, named by the URL on its assignment event, is still a
//! live instance. A holder found not live frees its issue at once: a
//! session registers itself before it can assign an issue, so a
//! not-live verdict means the holder has exited. A holder whose
//! liveness cannot be determined frees its issue once the assignment
//! is older than a day. Every issue left assigned is named in the
//! result with the reason and the assignment's age, so the user can
//! free it by hand with `gage issue open`.

use std::future::Future;
use std::pin::Pin;

use gage_core::datetime::{format_elapsed_ms, now_ms};
use gage_core::uuid::short_uuid;
use gage_store::{IssueChange, IssueFull, IssueStatus, IssueStore};
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

const HOUR_MS: i64 = 60 * 60 * 1000;

/// How old an assignment must be before an undetermined verdict frees it
const UNDETERMINED_TTL_MS: i64 = 24 * HOUR_MS;

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

/// Reopen every assigned issue whose holder is gone, as of now.
pub fn execute(config: &IssuesFreeConfig) -> CallToolResult {
    execute_at(config, now_ms())
}

/// [`execute`] with the thresholds measured from `now`.
fn execute_at(config: &IssuesFreeConfig, now: i64) -> CallToolResult {
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
        let Some(assignment) = assignment(issue) else {
            left.push(format!("{short}: no assignment event"));
            continue;
        };
        let age = now - assignment.timestamp_ms;
        let since = format_elapsed_ms(assignment.timestamp_ms);
        let verdict = match assignment.message.as_deref() {
            Some(url) => (config.session_live)(url),
            None => Err("no session recorded on the assignment".to_string()),
        };
        let message = match verdict {
            Ok(true) => {
                live += 1;
                continue;
            }
            Ok(false) => format!("freed: session is not live, assigned {since}"),
            Err(reason) if age < UNDETERMINED_TTL_MS => {
                left.push(format!(
                    "{short}: liveness undetermined ({reason}), assigned {since}, within 24h"
                ));
                continue;
            }
            Err(reason) => {
                format!("freed: liveness undetermined ({reason}), assigned {since}, past 24h")
            }
        };
        match issues.set_status(&issue.id, IssueStatus::Open, None, author, Some(&message)) {
            Ok(_) => freed.push(format!("{short} {}", issue.title)),
            Err(e) => left.push(format!("{short}: reopen failed: {e}")),
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

/// The issue's most recent assignment: the latest change that set
/// status to `assigned`. Its message is the holder's session URL.
fn assignment(issue: &IssueFull) -> Option<&IssueChange> {
    issue
        .changes
        .iter()
        .rev()
        .find(|change| change.to_status == Some(IssueStatus::Assigned))
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
        lines.push(format!("Left assigned: {}.", left.len()));
        lines.extend(left.iter().map(|u| format!("- {u}")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gage_store::IssueStore;

    use super::*;
    use crate::service::{SessionLiveFn, StoreWriterConfig};
    use crate::tools::store_test::{config, issue, text_of};

    fn status(writer: &StoreWriterConfig, id: &str) -> IssueStatus {
        let store = writer.store.lock().unwrap();
        IssueStore::from(&*store).get(id).unwrap().status
    }

    fn assign(writer: &StoreWriterConfig, id: &str, url: &str) {
        let store = writer.store.lock().unwrap();
        IssueStore::from(&*store)
            .assign(id, "user:t", url, false)
            .unwrap();
    }

    fn last_message(writer: &StoreWriterConfig, id: &str) -> String {
        let store = writer.store.lock().unwrap();
        let full = IssueStore::from(&*store).get(id).unwrap();
        let last = full.changes.last().unwrap();
        assert_eq!(last.to_status, Some(IssueStatus::Open));
        last.message.clone().unwrap()
    }

    /// Live for `live-*`, not live for `dead-*`, undetermined otherwise
    fn probe() -> SessionLiveFn {
        Arc::new(|url: &str| match url {
            u if u.starts_with("claude:/x/live-") => Ok(true),
            u if u.starts_with("claude:/x/dead-") => Ok(false),
            other => Err(format!("cannot probe {other}")),
        })
    }

    fn cfg(writer: &StoreWriterConfig) -> IssuesFreeConfig {
        IssuesFreeConfig {
            writer: writer.clone(),
            session_live: probe(),
        }
    }

    #[test]
    fn a_fresh_assignment_is_freed_at_once_when_its_holder_is_dead() {
        let (_tmp, writer) = config();
        let open = issue(&writer, "Plain open", IssueStatus::Open);
        let dead = issue(&writer, "Dead holder", IssueStatus::Open);
        let alive = issue(&writer, "Live holder", IssueStatus::Open);
        let murky = issue(&writer, "Unknown holder", IssueStatus::Open);
        assign(&writer, &dead, "claude:/x/dead-1");
        assign(&writer, &alive, "claude:/x/live-1");
        assign(&writer, &murky, "claude:/x/murky-1");

        let result = execute(&cfg(&writer));
        assert_eq!(result.is_error, Some(false), "{}", text_of(&result));
        let text = text_of(&result);
        assert!(text.contains("Freed 1:"), "{text}");
        assert!(text.contains("Dead holder"), "{text}");
        assert!(text.contains("Held by a live session: 1"), "{text}");
        assert!(text.contains("Left assigned: 1"), "{text}");
        assert!(text.contains("cannot probe claude:/x/murky-1"), "{text}");
        assert!(text.contains("within 24h"), "{text}");
        assert_eq!(status(&writer, &dead), IssueStatus::Open);
        assert_eq!(status(&writer, &alive), IssueStatus::Assigned);
        assert_eq!(status(&writer, &murky), IssueStatus::Assigned);
        assert_eq!(status(&writer, &open), IssueStatus::Open);
        assert!(
            last_message(&writer, &dead).starts_with("freed: session is not live"),
            "{}",
            last_message(&writer, &dead)
        );
    }

    #[test]
    fn an_undetermined_holder_is_freed_past_a_day() {
        let (_tmp, writer) = config();
        let murky = issue(&writer, "Unknown holder", IssueStatus::Open);
        let alive = issue(&writer, "Live holder", IssueStatus::Open);
        assign(&writer, &murky, "claude:/x/murky-1");
        assign(&writer, &alive, "claude:/x/live-1");

        let result = execute_at(&cfg(&writer), now_ms() + 23 * HOUR_MS);
        assert!(
            text_of(&result).contains("Freed no issues"),
            "{}",
            text_of(&result)
        );
        assert_eq!(status(&writer, &murky), IssueStatus::Assigned);

        let result = execute_at(&cfg(&writer), now_ms() + 25 * HOUR_MS);
        let text = text_of(&result);
        assert!(text.contains("Unknown holder"), "{text}");
        assert_eq!(status(&writer, &murky), IssueStatus::Open);
        assert_eq!(status(&writer, &alive), IssueStatus::Assigned);
        let message = last_message(&writer, &murky);
        assert!(
            message.contains("cannot probe claude:/x/murky-1"),
            "{message}"
        );
        assert!(message.ends_with("past 24h"), "{message}");
    }

    #[test]
    fn report_names_each_left_issue_by_its_own_reason() {
        let text = report(&[], 1, &["abc: reopen failed: locked".to_string()]);
        assert_eq!(
            text,
            "Freed no issues.\nHeld by a live session: 1.\nLeft assigned: 1.\n- abc: reopen failed: locked"
        );
    }

    #[test]
    fn no_assigned_issues_frees_nothing() {
        let (_tmp, writer) = config();
        issue(&writer, "Plain open", IssueStatus::Open);
        let result = execute(&cfg(&writer));
        assert_eq!(result.is_error, Some(false), "{}", text_of(&result));
        assert!(
            text_of(&result).contains("Freed no issues"),
            "{}",
            text_of(&result)
        );
    }
}
