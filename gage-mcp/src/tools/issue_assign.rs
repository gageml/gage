//! `issue_assign`: assign an open issue to this session.

use std::future::Future;
use std::pin::Pin;

use gage_core::uuid::short_uuid;
use gage_store::{IssueFull, IssueStatus, IssueStore, StoreError};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};

use super::issue_update::optional_string;
use crate::server::GageServer;
use crate::service::IssueAssignConfig;
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};

pub const NAME: &str = "issue_assign";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/issue_assign.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "issue_assign description exceeds Claude Code's 2048-byte cap",
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
        let Some(config) = &ctx.service.config().issue_assign else {
            return Err(McpError::internal_error(
                "issue_assign tool is routed but not configured",
                None,
            ));
        };
        let harness =
            ctx.request_context.peer.peer_info().map(|client| {
                format!("{} {}", client.client_info.name, client.client_info.version)
            });
        // rmcp moves the request's `_meta` into the request context
        // before dispatch
        execute(
            config,
            &ctx.request_context.meta.0,
            harness.as_deref(),
            params,
        )
    })
}

/// Why a call is refused when its session cannot be named. `harness`
/// is the name and version the harness gave for itself when it
/// connected, which tells the reader which harness names no session.
fn no_session(harness: Option<&str>) -> String {
    let harness = match harness {
        Some(harness) => format!("The harness ({harness})"),
        None => "The harness".to_string(),
    };
    format!(
        "issue_assign could not identify this session, so it has no holder to record. \
         {harness} did not name the session in the server's environment or in the call."
    )
}

/// What one call asks for, parsed
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// The first open issue of the list
    Pool(Vec<String>),
    /// One issue, reassigned when held elsewhere only with `force`
    Issue { id: String, force: bool },
}

/// Parse the call and apply it for the session the call comes from,
/// named from the call's request `_meta`. `harness` is what the
/// harness making the call said it is, when it said. A call that
/// violates the declared parameters is an invalid-params error; a
/// store refusal, and a session that cannot be named, is a result the
/// model reads.
pub fn execute(
    config: &IssueAssignConfig,
    meta: &JsonObject,
    harness: Option<&str>,
    params: JsonObject,
) -> Result<CallToolResult, McpError> {
    let request = parse_request(&params)?;
    let session = match (config.current_session)(meta) {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(domain_error(no_session(harness))),
        Err(reason) => {
            return Ok(domain_error(format!("{} ({reason})", no_session(harness))));
        }
    };
    let session = session.as_str();
    let store = config.writer.store();
    let issues = IssueStore::from(&*store);
    let author = &config.writer.author;
    match request {
        Request::Pool(pool) => {
            for id in &pool {
                let issue = match issues.get(id) {
                    Ok(issue) => issue,
                    Err(e) => return Ok(domain_error(format!("pool: {e}"))),
                };
                if issue.status != IssueStatus::Open {
                    continue;
                }
                match issues.assign(&issue.id, author, session, false) {
                    Ok(_) => return Ok(success(assigned(&issue))),
                    // The ref moved between the read and the write:
                    // another session took the issue first
                    Err(StoreError::Git { .. }) => continue,
                    Err(e) => return Ok(domain_error(e.to_string())),
                }
            }
            Ok(success("No open issue in pool.".to_string()))
        }
        Request::Issue { id, force } => {
            let issue = match issues.get(&id) {
                Ok(issue) => issue,
                Err(e) => return Ok(domain_error(e.to_string())),
            };
            let held = issue.status == IssueStatus::Assigned;
            match issues.assign(&issue.id, author, session, force) {
                Ok(_) if held => Ok(success(format!(
                    "Reassigned issue {} ({}) to this session.",
                    short_uuid(&issue.id),
                    issue.title
                ))),
                Ok(_) => Ok(success(assigned(&issue))),
                Err(e) if held && !force => {
                    Ok(domain_error(format!("{e}; set force to reassign it")))
                }
                Err(e) => Ok(domain_error(e.to_string())),
            }
        }
    }
}

fn assigned(issue: &IssueFull) -> String {
    format!(
        "Assigned issue {} ({}).",
        short_uuid(&issue.id),
        issue.title
    )
}

fn parse_request(params: &JsonObject) -> Result<Request, McpError> {
    let pool = match params.get("pool") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(items)) => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(id) if !id.is_empty() => ids.push(id.to_string()),
                    _ => {
                        return Err(McpError::invalid_params(
                            "`pool` items must be non-empty strings",
                            None,
                        ));
                    }
                }
            }
            Some(ids)
        }
        Some(_) => {
            return Err(McpError::invalid_params(
                "`pool` must be an array of issue IDs",
                None,
            ));
        }
    };
    let issue = optional_string(params, "issue")?;
    let force = match params.get("force") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return Err(McpError::invalid_params("`force` must be a boolean", None)),
    };
    match (pool, issue) {
        (Some(pool), None) => {
            if force {
                return Err(McpError::invalid_params(
                    "`force` applies to `issue`, not `pool`",
                    None,
                ));
            }
            if pool.is_empty() {
                return Err(McpError::invalid_params("`pool` is empty", None));
            }
            Ok(Request::Pool(pool))
        }
        (None, Some(id)) => Ok(Request::Issue { id, force }),
        _ => Err(McpError::invalid_params(
            "give exactly one of `pool` or `issue`",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::service::StoreWriterConfig;
    use crate::tools::store_test::{config, issue, object, text_of};

    fn status(config: &StoreWriterConfig, id: &str) -> IssueStatus {
        let store = config.store.lock().unwrap();
        IssueStore::from(&*store).get(id).unwrap().status
    }

    /// The tool as session `session_url` would run it
    fn session(writer: &StoreWriterConfig, session_url: &str) -> IssueAssignConfig {
        let session_url = session_url.to_string();
        IssueAssignConfig {
            writer: writer.clone(),
            current_session: Arc::new(move |_meta| Ok(Some(session_url.clone()))),
        }
    }

    /// `execute` for a call with no request `_meta`
    fn execute(config: &IssueAssignConfig, params: JsonObject) -> Result<CallToolResult, McpError> {
        super::execute(config, &JsonObject::new(), None, params)
    }

    #[test]
    fn the_session_is_named_from_the_call_meta() {
        let (_tmp, writer) = config();
        let config = IssueAssignConfig {
            writer: writer.clone(),
            current_session: Arc::new(|meta| {
                Ok(meta
                    .get("session")
                    .and_then(|s| s.as_str())
                    .map(String::from))
            }),
        };
        let id = issue(&writer, "Something", IssueStatus::Open);

        let r = super::execute(
            &config,
            &JsonObject::new(),
            Some("pi 1.5.2"),
            object(json!({"issue": id})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
        assert!(
            text_of(&r).contains("could not identify this session"),
            "{}",
            text_of(&r)
        );
        assert!(
            text_of(&r).contains("The harness (pi 1.5.2) did not name the session"),
            "{}",
            text_of(&r)
        );
        assert_eq!(status(&writer, &id), IssueStatus::Open);

        let meta = object(json!({"session": "session a"}));
        let r = super::execute(&config, &meta, None, object(json!({"issue": id}))).unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        let store = writer.store.lock().unwrap();
        let full = IssueStore::from(&*store).get(&id).unwrap();
        assert_eq!(
            full.changes.last().unwrap().message.as_deref(),
            Some("session a")
        );
    }

    #[test]
    fn a_failure_to_name_the_session_is_reported_with_its_reason() {
        let (_tmp, writer) = config();
        let config = IssueAssignConfig {
            writer: writer.clone(),
            current_session: Arc::new(|_meta| Err("source unavailable".to_string())),
        };
        let id = issue(&writer, "Something", IssueStatus::Open);
        let r = execute(&config, object(json!({"issue": id}))).unwrap();
        assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
        assert!(
            text_of(&r).contains("source unavailable"),
            "{}",
            text_of(&r)
        );
        assert_eq!(status(&writer, &id), IssueStatus::Open);
    }

    #[test]
    fn pool_assigns_the_first_open_issue_and_passes_over_the_rest() {
        let (_tmp, config) = config();
        let held = issue(&config, "Held", IssueStatus::Open);
        let pending = issue(&config, "Pending", IssueStatus::Pending);
        let first = issue(&config, "First open", IssueStatus::Open);
        let second = issue(&config, "Second open", IssueStatus::Open);
        {
            let store = config.store.lock().unwrap();
            IssueStore::from(&*store)
                .assign(&held, "user:t", "session a", false)
                .unwrap();
        }

        let pool = json!([held, pending, first, second]);
        let r = execute(
            &session(&config, "session b"),
            object(json!({"pool": pool})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        assert!(text_of(&r).contains("First open"), "{}", text_of(&r));
        assert_eq!(status(&config, &first), IssueStatus::Assigned);
        assert_eq!(status(&config, &second), IssueStatus::Open);
        {
            let store = config.store.lock().unwrap();
            let full = IssueStore::from(&*store).get(&first).unwrap();
            assert_eq!(
                full.changes.last().unwrap().message.as_deref(),
                Some("session b")
            );
        }

        let r = execute(
            &session(&config, "session c"),
            object(json!({"pool": pool})),
        )
        .unwrap();
        assert!(text_of(&r).contains("Second open"), "{}", text_of(&r));

        let r = execute(
            &session(&config, "session d"),
            object(json!({"pool": pool})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        assert_eq!(text_of(&r), "No open issue in pool.");
    }

    #[test]
    fn issue_refuses_a_held_issue_unless_forced() {
        let (_tmp, config) = config();
        let id = issue(&config, "Something", IssueStatus::Open);

        let r = execute(&session(&config, "session a"), object(json!({"issue": id}))).unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        assert_eq!(status(&config, &id), IssueStatus::Assigned);

        let r = execute(&session(&config, "session b"), object(json!({"issue": id}))).unwrap();
        assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
        assert!(text_of(&r).contains("set force"), "{}", text_of(&r));

        let r = execute(
            &session(&config, "session b"),
            object(json!({"issue": id, "force": true})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        assert!(text_of(&r).starts_with("Reassigned"), "{}", text_of(&r));
        {
            let store = config.store.lock().unwrap();
            let full = IssueStore::from(&*store).get(&id).unwrap();
            assert_eq!(
                full.changes.last().unwrap().message.as_deref(),
                Some("session b")
            );
        }

        let closed = issue(&config, "Closed", IssueStatus::Open);
        {
            let store = config.store.lock().unwrap();
            IssueStore::from(&*store)
                .set_status(&closed, IssueStatus::Closed, None, "user:t", None)
                .unwrap();
        }
        let r = execute(
            &session(&config, "session a"),
            object(json!({"issue": closed})),
        )
        .unwrap();
        assert_eq!(r.is_error, Some(true), "{}", text_of(&r));
        assert!(text_of(&r).contains("not open"), "{}", text_of(&r));
    }

    #[test]
    fn exactly_one_of_pool_or_issue() {
        let (_tmp, config) = config();
        let id = issue(&config, "Something", IssueStatus::Open);
        assert!(execute(&session(&config, "s"), object(json!({}))).is_err());
        assert!(
            execute(
                &session(&config, "s"),
                object(json!({"pool": [id], "issue": id}))
            )
            .is_err()
        );
        assert!(execute(&session(&config, "s"), object(json!({"pool": []}))).is_err());
        assert!(
            execute(
                &session(&config, "s"),
                object(json!({"pool": [id], "force": true}))
            )
            .is_err()
        );
    }
}
