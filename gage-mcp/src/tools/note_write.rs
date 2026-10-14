//! `NoteWrite`: one note written through the configured callback,
//! under one of the configured names.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};

use crate::server::GageServer;
use crate::service::{NoteWriteConfig, NoteWriteInput, call_result};
use crate::tool::{MAX_DESCRIPTION_BYTES, build_tool_meta, description_byte_len, domain_error};
use crate::tools::issue_update::{optional_string, required_string};

pub const NAME: &str = "NoteWrite";

const MD: &str = include_str!("../../config/tools/NoteWrite.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "NoteWrite description exceeds Claude Code's 4096-byte cap",
);

/// The route for one configuration. The description ends with the
/// allowed `name` values and their docs, which are per configuration
/// and so cannot sit in the `.md`.
pub fn route(config: &NoteWriteConfig) -> ToolRoute<GageServer> {
    let mut meta = build_tool_meta(MD);
    let base = meta.description.take().unwrap_or_default();
    meta.description = Some(describe(&base, &config.names).into());
    ToolRoute::new_dyn(meta, |ctx: ToolCallContext<'_, GageServer>| {
        Box::pin(handle(ctx))
    })
}

fn describe(base: &str, names: &BTreeMap<String, String>) -> String {
    let mut text = format!("{base}\n\nAllowed `name` values:\n");
    for (name, doc) in names {
        if doc.is_empty() {
            text.push_str(&format!("- `{name}`\n"));
        } else {
            text.push_str(&format!("- `{name}`: {doc}\n"));
        }
    }
    text
}

fn handle(
    ctx: ToolCallContext<'_, GageServer>,
) -> Pin<Box<dyn Future<Output = Result<CallToolResult, McpError>> + Send + '_>> {
    Box::pin(async move {
        let params = ctx.arguments.unwrap_or_default();
        let Some(config) = &ctx.service.config().note_write else {
            return Err(McpError::internal_error(
                "NoteWrite tool is routed but not configured",
                None,
            ));
        };
        execute(config, params).await
    })
}

/// Parse the call, check the name against the configured names, and
/// run the callback. A call that violates the declared parameters is
/// an invalid-params error; a name outside the configuration is a
/// result the model reads, listing the names it may use.
pub async fn execute(
    config: &NoteWriteConfig,
    params: JsonObject,
) -> Result<CallToolResult, McpError> {
    let input = parse(&params)?;
    if !config.names.contains_key(&input.name) {
        let allowed: Vec<String> = config.names.keys().map(|n| format!("`{n}`")).collect();
        return Ok(domain_error(format!(
            "`name` must be one of {}; got `{}`",
            allowed.join(", "),
            input.name
        )));
    }
    call_result((config.callback)(input).await)
}

fn parse(params: &JsonObject) -> Result<NoteWriteInput, McpError> {
    let session_lines = match params.get("session_lines") {
        // A single line arrives as a number from some callers
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => optional_string(params, "session_lines")?,
    };
    Ok(NoteWriteInput {
        name: required_string(params, "name")?,
        value: required_string(params, "value")?,
        session_id: optional_string(params, "session_id")?,
        session_lines,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::service::CustomToolOutcome;
    use crate::tools::store_test::{object, text_of};

    /// A config over `names` whose callback records the input it
    /// received and answers with a fixed success
    fn recording(names: &[(&str, &str)]) -> (NoteWriteConfig, Arc<Mutex<Vec<NoteWriteInput>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let config = NoteWriteConfig {
            names: names
                .iter()
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .collect(),
            callback: Arc::new(move |input| {
                record.lock().unwrap().push(input);
                Box::pin(async { CustomToolOutcome::Success(json!("ok")) })
            }),
        };
        (config, seen)
    }

    #[test]
    fn the_description_lists_the_allowed_names() {
        let (config, _) = recording(&[("finding", "A finding"), ("summary", "")]);
        let route = route(&config);
        let description = route.attr.description.as_deref().unwrap().to_string();
        assert!(
            description.contains("Use to record a note"),
            "{description}"
        );
        assert!(
            description.ends_with("Allowed `name` values:\n- `finding`: A finding\n- `summary`\n"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn a_known_name_reaches_the_callback_and_an_unknown_one_is_refused() {
        let (config, seen) = recording(&[("finding", "A finding")]);
        let r = execute(
            &config,
            object(json!({"name": "finding", "value": "it flakes", "session_id": "abc", "session_lines": 12})),
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(false), "{}", text_of(&r));
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [NoteWriteInput {
                name: "finding".into(),
                value: "it flakes".into(),
                session_id: Some("abc".into()),
                session_lines: Some("12".into()),
            }]
        );
        let r = execute(&config, object(json!({"name": "other", "value": "x"})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r), "`name` must be one of `finding`; got `other`");
        assert!(
            execute(&config, object(json!({"name": "finding"})))
                .await
                .is_err()
        );
    }
}
