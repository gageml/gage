//! `IssuePendingResolve`: apply a whole pending-issue resolution plan.
//!
//! The plan is validated as a unit before any write, so an invalid
//! plan changes nothing. The store commits each issue independently;
//! there is no cross-issue transaction, so a write that fails after
//! validation leaves the earlier writes applied and the result says
//! how far it got.

use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_store::{IssueStatus, IssueStore};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::Value;

use crate::server::GageServer;
use crate::service::StoreWriterConfig;
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};
use crate::tools::issue_update::optional_string;

pub const NAME: &str = "IssuePendingResolve";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssuePendingResolve.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "IssuePendingResolve description exceeds Claude Code's 4096-byte cap",
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
        let Some(config) = &ctx.service.config().issue_pending_resolve else {
            return Err(McpError::internal_error(
                "IssuePendingResolve tool is routed but not configured",
                None,
            ));
        };
        execute(config, params)
    })
}

/// One disposition as the call gives it, before id resolution
struct Resolution {
    issue: String,
    action: Action,
}

enum Action {
    Open,
    Duplicate {
        of: String,
        comment: Option<String>,
        reopen: bool,
    },
}

/// A disposition with its subject and target resolved to full ids
enum Planned {
    Open {
        issue: String,
    },
    Duplicate {
        issue: String,
        of: String,
        comment: Option<String>,
        reopen: bool,
    },
}

impl Planned {
    fn issue(&self) -> &str {
        match self {
            Planned::Open { issue } | Planned::Duplicate { issue, .. } => issue,
        }
    }

    fn is_open(&self) -> bool {
        matches!(self, Planned::Open { .. })
    }
}

/// Parse and apply the plan. A call that violates the declared
/// parameters is an invalid-params error. A plan that fails validation
/// is a result the model reads, and nothing is written. A write that
/// fails after validation is also a result the model reads, carrying
/// how much of the plan was applied.
pub fn execute(config: &StoreWriterConfig, params: JsonObject) -> Result<CallToolResult, McpError> {
    let resolutions = parse_resolutions(&params)?;
    let store = config.store();
    let issues = IssueStore::from(&*store);

    let plan = match validate(&issues, &resolutions) {
        Ok(plan) => plan,
        Err(msg) => return Ok(domain_error(msg)),
    };
    Ok(apply(&issues, &plan, &config.author))
}

fn parse_resolutions(params: &JsonObject) -> Result<Vec<Resolution>, McpError> {
    let Some(Value::Array(items)) = params.get("resolutions") else {
        return Err(McpError::invalid_params(
            "`resolutions` must be an array of disposition objects",
            None,
        ));
    };
    if items.is_empty() {
        return Err(McpError::invalid_params(
            "`resolutions` is empty; name at least one pending issue",
            None,
        ));
    }
    items.iter().map(parse_resolution).collect()
}

fn parse_resolution(item: &Value) -> Result<Resolution, McpError> {
    let Value::Object(obj) = item else {
        return Err(McpError::invalid_params(
            "each resolution must be an object",
            None,
        ));
    };
    let issue = required_field(obj, "issue")?;
    let action = required_field(obj, "action")?;
    let action = match action.as_str() {
        "open" => {
            if obj.contains_key("of") {
                return Err(McpError::invalid_params(
                    "`of` applies to a duplicate, not to open",
                    None,
                ));
            }
            Action::Open
        }
        "duplicate" => {
            let Some(of) = optional_string(obj, "of")? else {
                return Err(McpError::invalid_params(
                    "a duplicate names the surviving issue in `of`",
                    None,
                ));
            };
            Action::Duplicate {
                of,
                comment: optional_string(obj, "comment")?,
                reopen: parse_reopen(obj)?,
            }
        }
        other => {
            return Err(McpError::invalid_params(
                format!("`action` must be open or duplicate, not `{other}`"),
                None,
            ));
        }
    };
    Ok(Resolution { issue, action })
}

fn parse_reopen(obj: &JsonObject) -> Result<bool, McpError> {
    match obj.get("reopen") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(McpError::invalid_params("`reopen` must be a boolean", None)),
    }
}

fn required_field(obj: &JsonObject, key: &str) -> Result<String, McpError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| McpError::invalid_params(format!("each resolution needs `{key}`"), None))
}

/// Resolve every id and check the plan as a whole. The returned error
/// string names the offending resolution and is shown to the model.
fn validate(issues: &IssueStore<'_>, resolutions: &[Resolution]) -> Result<Vec<Planned>, String> {
    let mut plan: Vec<Planned> = Vec::with_capacity(resolutions.len());
    for r in resolutions {
        let subject = issues.get(&r.issue).map_err(|e| e.to_string())?;
        if subject.status != IssueStatus::Pending {
            return Err(format!(
                "issue {} is not pending (status {})",
                short_uuid(&subject.id),
                subject.status.as_str()
            ));
        }
        if plan.iter().any(|p| p.issue() == subject.id) {
            return Err(format!(
                "issue {} appears more than once",
                short_uuid(&subject.id)
            ));
        }
        plan.push(match &r.action {
            Action::Open => Planned::Open { issue: subject.id },
            Action::Duplicate {
                of,
                comment,
                reopen,
            } => Planned::Duplicate {
                issue: subject.id,
                of: issues.get(of).map_err(|e| format!("of: {e}"))?.id,
                comment: comment.clone(),
                reopen: *reopen,
            },
        });
    }

    for p in &plan {
        let Planned::Duplicate {
            issue, of, reopen, ..
        } = p
        else {
            continue;
        };
        if of == issue {
            return Err(format!(
                "issue {} closed as a duplicate of itself",
                short_uuid(issue)
            ));
        }
        if let Some(target) = plan.iter().find(|t| t.issue() == *of) {
            // The target is disposed in this plan: valid only as a
            // promoted survivor. A target closed as a duplicate is a
            // chain the caller must flatten.
            if !target.is_open() {
                return Err(format!(
                    "issue {} closes against {}, which itself closes as a duplicate; \
                     flatten the chain to the final target",
                    short_uuid(issue),
                    short_uuid(of)
                ));
            }
            if *reopen {
                return Err(format!(
                    "issue {} sets reopen for {}, which this plan promotes rather than closes",
                    short_uuid(issue),
                    short_uuid(of)
                ));
            }
            continue;
        }
        let target = issues.get(of).map_err(|e| format!("of: {e}"))?;
        match target.status {
            IssueStatus::Open | IssueStatus::Resolving => {
                if *reopen {
                    return Err(format!(
                        "issue {} sets reopen for {}, which is not closed",
                        short_uuid(issue),
                        short_uuid(of)
                    ));
                }
            }
            IssueStatus::Pending => {
                return Err(format!(
                    "issue {} closes against pending issue {}, which this plan does not promote",
                    short_uuid(issue),
                    short_uuid(of)
                ));
            }
            IssueStatus::Closed => {}
            IssueStatus::Unknown => {
                return Err(format!(
                    "issue {} closes against issue {}, whose status this build does not read",
                    short_uuid(issue),
                    short_uuid(of)
                ));
            }
        }
    }
    Ok(plan)
}

/// Counts of what an apply wrote, carried into the result summary
#[derive(Default, Clone, Copy)]
struct Counts {
    promoted: usize,
    reopened: usize,
    closed: usize,
    comments: usize,
}

/// Apply the validated plan: promotions, then reopens, then closes,
/// then comments, so a survivor reaches its final state before a
/// duplicate names it. The first write error ends the apply and is
/// reported with the counts applied so far.
fn apply(issues: &IssueStore<'_>, plan: &[Planned], author: &str) -> CallToolResult {
    let mut counts = Counts::default();
    let mut reopened: Vec<&str> = Vec::new();

    for p in plan {
        if let Planned::Open { issue } = p {
            if let Err(e) = issues.set_status(issue, IssueStatus::Open, None, author, None) {
                return apply_failed(issues, "promoting", issue, &e.to_string(), counts);
            }
            counts.promoted += 1;
        }
    }
    for p in plan {
        if let Planned::Duplicate {
            of, reopen: true, ..
        } = p
            && !reopened.contains(&of.as_str())
        {
            if let Err(e) = issues.set_status(of, IssueStatus::Open, None, author, None) {
                return apply_failed(issues, "reopening", of, &e.to_string(), counts);
            }
            reopened.push(of);
            counts.reopened += 1;
        }
    }
    for p in plan {
        if let Planned::Duplicate { issue, of, .. } = p {
            if let Err(e) = issues.close_duplicate(issue, of, author, None) {
                return apply_failed(issues, "closing", issue, &e.to_string(), counts);
            }
            counts.closed += 1;
        }
    }
    for p in plan {
        if let Planned::Duplicate {
            of,
            comment: Some(comment),
            ..
        } = p
        {
            if let Err(e) = issues.comment(of, author, comment) {
                return apply_failed(issues, "commenting on", of, &e.to_string(), counts);
            }
            counts.comments += 1;
        }
    }
    success(apply_summary(issues, counts))
}

fn apply_summary(issues: &IssueStore<'_>, counts: Counts) -> String {
    let pending = issues
        .query()
        .status(IssueStatus::Pending)
        .count()
        .map(|n| format!(" {n} pending remain."))
        .unwrap_or_default();
    format!(
        "Promoted {}, closed {} as duplicates, reopened {}, added {} comments.{pending}",
        counts.promoted, counts.closed, counts.reopened, counts.comments
    )
}

fn apply_failed(
    issues: &IssueStore<'_>,
    stage: &str,
    id: &str,
    error: &str,
    counts: Counts,
) -> CallToolResult {
    domain_error(format!(
        "{stage} issue {} failed: {error}. Applied so far: {}",
        short_uuid(id),
        apply_summary(issues, counts)
    ))
}

#[cfg(test)]
mod tests {
    use gage_store::StatusReason;
    use serde_json::json;

    use super::*;
    use crate::tools::store_test::{config, issue, object, text_of};

    #[test]
    fn promotes_closes_reopens_and_comments_in_one_plan() {
        let (_tmp, cfg) = config();
        let novel = issue(&cfg, "Novel condition", IssueStatus::Pending);
        let dup = issue(&cfg, "Same as live", IssueStatus::Pending);
        let recur = issue(&cfg, "Recurred", IssueStatus::Pending);
        let live = issue(&cfg, "Live issue", IssueStatus::Open);
        let done = issue(&cfg, "Closed issue", IssueStatus::Open);
        {
            let store = cfg.store.lock().unwrap();
            IssueStore::from(&*store)
                .set_status(&done, IssueStatus::Closed, None, "user:t", None)
                .unwrap();
        }

        let r = execute(
            &cfg,
            object(json!({"resolutions": [
                {"issue": novel, "action": "open"},
                {"issue": dup, "action": "duplicate", "of": live, "comment": "insight"},
                {"issue": recur, "action": "duplicate", "of": done, "reopen": true},
            ]})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));

        let store = cfg.store.lock().unwrap();
        let issues = IssueStore::from(&*store);
        assert_eq!(issues.get(&novel).unwrap().status, IssueStatus::Open);
        let dup_full = issues.get(&dup).unwrap();
        assert_eq!(dup_full.status, IssueStatus::Closed);
        assert_eq!(dup_full.status_reason, Some(StatusReason::Duplicate));
        assert_eq!(
            dup_full.changes.last().unwrap().of.as_deref(),
            Some(live.as_str())
        );
        assert_eq!(issues.get(&live).unwrap().status, IssueStatus::Open);
        // The recurred survivor was reopened before the duplicate closed against it
        assert_eq!(issues.get(&done).unwrap().status, IssueStatus::Open);
        assert_eq!(issues.get(&recur).unwrap().status, IssueStatus::Closed);
    }

    #[test]
    fn a_non_pending_subject_writes_nothing() {
        let (_tmp, cfg) = config();
        let open = issue(&cfg, "Already open", IssueStatus::Open);
        let pending = issue(&cfg, "Pending", IssueStatus::Pending);
        let r = execute(
            &cfg,
            object(json!({"resolutions": [
                {"issue": pending, "action": "open"},
                {"issue": open, "action": "open"},
            ]})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
        let store = cfg.store.lock().unwrap();
        // The valid first disposition was not applied: validation failed whole
        assert_eq!(
            IssueStore::from(&*store).get(&pending).unwrap().status,
            IssueStatus::Pending
        );
    }

    #[test]
    fn a_chain_is_rejected() {
        let (_tmp, cfg) = config();
        let a = issue(&cfg, "A", IssueStatus::Pending);
        let b = issue(&cfg, "B", IssueStatus::Pending);
        let live = issue(&cfg, "Live", IssueStatus::Open);
        let r = execute(
            &cfg,
            object(json!({"resolutions": [
                {"issue": a, "action": "duplicate", "of": b},
                {"issue": b, "action": "duplicate", "of": live},
            ]})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).contains("flatten the chain"), "{}", text_of(&r));
    }

    #[test]
    fn malformed_calls_are_invalid_params() {
        let (_tmp, cfg) = config();
        let id = issue(&cfg, "Pending", IssueStatus::Pending);
        for params in [
            json!({}),
            json!({"resolutions": []}),
            json!({"resolutions": [{"issue": id, "action": "maybe"}]}),
            json!({"resolutions": [{"issue": id, "action": "duplicate"}]}),
            json!({"resolutions": [{"action": "open"}]}),
        ] {
            assert!(execute(&cfg, object(params.clone())).is_err(), "{params}");
        }
    }
}
