//! `IssueWrite`: one issue written through the configured callback.

use std::future::Future;
use std::pin::Pin;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};

use crate::server::GageServer;
use crate::service::{IssueWriteConfig, IssueWriteInput, call_result};
use crate::tool::{MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len};

pub const NAME: &str = "IssueWrite";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/IssueWrite.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "IssueWrite description exceeds Claude Code's 2048-byte cap",
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
        let Some(config) = &ctx.service.config().issue_write else {
            return Err(McpError::internal_error(
                "IssueWrite tool is routed but not configured",
                None,
            ));
        };
        execute(config, params).await
    })
}

/// Parse the call and run the callback. A call that violates the
/// declared parameters is an invalid-params error; what the callback
/// returns is rendered as any tool outcome is.
pub async fn execute(
    config: &IssueWriteConfig,
    params: JsonObject,
) -> Result<CallToolResult, McpError> {
    let input = parse(&params)?;
    call_result((config.callback)(input).await)
}

fn parse(params: &JsonObject) -> Result<IssueWriteInput, McpError> {
    let title = params
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| McpError::invalid_params("missing or empty `title`", None))?
        .to_string();
    let description = match params.get("description") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(McpError::invalid_params(
                "`description` must be a string",
                None,
            ));
        }
    };
    let evidence = match params.get("evidence") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let id = item.as_str().ok_or_else(|| {
                    McpError::invalid_params(format!("`evidence[{i}]` must be a string"), None)
                })?;
                out.push(id.to_string());
            }
            out
        }
        Some(_) => {
            return Err(McpError::invalid_params(
                "`evidence` must be an array",
                None,
            ));
        }
    };
    Ok(IssueWriteInput {
        title,
        description,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::service::CustomToolOutcome;

    /// A config whose callback records the input it received and
    /// answers with `outcome`.
    fn recording(
        outcome: CustomToolOutcome,
    ) -> (IssueWriteConfig, Arc<Mutex<Vec<IssueWriteInput>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let config = IssueWriteConfig {
            callback: Arc::new(move |input| {
                record.lock().unwrap().push(input);
                let outcome = outcome.clone();
                Box::pin(async move { outcome })
            }),
        };
        (config, seen)
    }

    fn object(v: serde_json::Value) -> JsonObject {
        match v {
            serde_json::Value::Object(o) => o,
            other => panic!("not an object: {other}"),
        }
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    #[tokio::test]
    async fn a_call_is_parsed_and_the_callback_outcome_is_the_result() {
        let (config, seen) = recording(CustomToolOutcome::Success(json!("Wrote issue")));
        let r = execute(
            &config,
            object(json!({
                "title": "Flaky build",
                "description": "## Summary\nIt flakes.",
                "evidence": ["n1", "n2"],
            })),
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(false));
        assert_eq!(text_of(&r), "Wrote issue");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [IssueWriteInput {
                title: "Flaky build".into(),
                description: Some("## Summary\nIt flakes.".into()),
                evidence: vec!["n1".into(), "n2".into()],
            }]
        );
    }

    #[tokio::test]
    async fn absent_or_empty_optional_fields_are_none_and_empty() {
        let (config, seen) = recording(CustomToolOutcome::Success(json!("ok")));
        execute(
            &config,
            object(json!({"title": "t", "description": "", "evidence": null})),
        )
        .await
        .unwrap();
        assert_eq!(
            seen.lock().unwrap()[0],
            IssueWriteInput {
                title: "t".into(),
                description: None,
                evidence: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn bad_parameters_are_invalid_params_and_the_callback_is_not_called() {
        let (config, seen) = recording(CustomToolOutcome::Success(json!("ok")));
        for (params, message) in [
            (json!({}), "missing or empty `title`"),
            (json!({"title": "  "}), "missing or empty `title`"),
            (
                json!({"title": "t", "evidence": "n1"}),
                "`evidence` must be an array",
            ),
            (
                json!({"title": "t", "evidence": [1]}),
                "`evidence[0]` must be a string",
            ),
            (
                json!({"title": "t", "description": 5}),
                "`description` must be a string",
            ),
        ] {
            let e = execute(&config, object(params)).await.unwrap_err();
            assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert_eq!(e.message, message);
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn callback_errors_and_faults_keep_their_levels() {
        let (config, _) = recording(CustomToolOutcome::Error("note x is deleted".into()));
        let r = execute(&config, object(json!({"title": "t"})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r), "note x is deleted");
        let (config, _) = recording(CustomToolOutcome::Fault("staging gone".into()));
        let e = execute(&config, object(json!({"title": "t"})))
            .await
            .unwrap_err();
        assert_eq!(e.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn wire_definition_comes_from_the_md() {
        let meta = build_tool_meta(MD);
        assert_eq!(meta.name, NAME);
        let annotations = meta.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(false));
        assert_eq!(
            meta.input_schema.get("required").unwrap(),
            &json!(["title"])
        );
        let evidence = meta
            .input_schema
            .get("properties")
            .and_then(|p| p.get("evidence"))
            .unwrap();
        assert_eq!(evidence.get("type"), Some(&json!("array")));
        assert_eq!(evidence.get("items"), Some(&json!({"type": "string"})));
    }
}
