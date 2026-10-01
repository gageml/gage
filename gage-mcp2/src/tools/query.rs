//! `Query`: SQL over the context the service was configured with.

use std::future::Future;
use std::pin::Pin;

use datafusion::prelude::SessionContext;
use gage_query::write_yaml_capped;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;

use crate::server::GageServer;
use crate::tool::{MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len};

pub const NAME: &str = "Query";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/Query.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "Query description exceeds Claude Code's 2048-byte cap",
);

/// The most result text one call returns; rows past it are cut and
/// the model is told how to continue
const PAGE_CAP_BYTES: usize = 45_000;

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
        let sql = params
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::invalid_params("missing or non-string `sql`", None))?;
        let Some(config) = &ctx.service.config().query else {
            return Err(McpError::internal_error(
                "Query tool is routed but not configured",
                None,
            ));
        };
        execute(&config.context, sql).await
    })
}

/// Run `sql` and render the rows as a page of YAML. SQL and execution
/// errors are results the model reads; a serialization failure is a
/// protocol error.
pub async fn execute(context: &SessionContext, sql: &str) -> Result<CallToolResult, McpError> {
    let df = match context.sql(sql).await {
        Ok(df) => df,
        Err(e) => return Ok(domain_error(format!("SQL error: {e}"))),
    };
    let batches = match df.collect().await {
        Ok(b) => b,
        Err(e) => return Ok(domain_error(format!("query execution error: {e}"))),
    };
    let batches: Vec<_> = batches
        .iter()
        .filter(|b| b.num_rows() > 0)
        .cloned()
        .collect();
    let row_count: usize = batches.iter().map(|b| b.num_rows()).sum();
    if batches.is_empty() {
        return Ok(success("0 rows"));
    }
    let mut buf: Vec<u8> = b"```yaml\n".to_vec();
    let rows_written = write_yaml_capped(&mut buf, &batches, PAGE_CAP_BYTES)
        .map_err(|e| McpError::internal_error(format!("YAML serialization error: {e}"), None))?;
    buf.extend_from_slice(b"\n```\n");
    let yaml = String::from_utf8(buf)
        .map_err(|e| McpError::internal_error(format!("UTF-8 error: {e}"), None))?;
    if rows_written == row_count {
        return Ok(success(yaml));
    }
    if rows_written == 0 {
        let msg = json!({
            "error": "single row exceeds page cap",
            "cap_bytes": PAGE_CAP_BYTES,
            "suggestion": "SELECT substr(text, 1, 800) or substr(raw, 1, 800) \
                           instead of the full column, or omit wide columns \
                           (text, raw) entirely.",
        })
        .to_string();
        return Ok(domain_error(msg));
    }
    Ok(success(format!(
        "TRUNCATED: showing rows 1-{rows_written} of {row_count} \
         (page cap {PAGE_CAP_BYTES} bytes)\n{yaml}\
         To continue, re-run the query with `OFFSET {rows_written}` appended \
         (stable only under a deterministic ORDER BY), or preferably use a \
         keyset predicate on the ordered column (e.g. `AND line > <last line \
         shown>`)."
    )))
}

fn success(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn domain_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::arrow::array::{Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;

    use super::*;

    fn context() -> SessionContext {
        let schema = Arc::new(Schema::new(vec![
            Field::new("line", DataType::Int64, false),
            Field::new("text", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["hello", "world"])),
            ],
        )
        .unwrap();
        let ctx = SessionContext::new();
        ctx.register_table(
            "message",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        ctx
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    #[tokio::test]
    async fn rows_render_as_yaml() {
        let r = execute(&context(), "SELECT line, text FROM message ORDER BY line")
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(false));
        assert_eq!(
            text_of(&r),
            "```yaml\nline: 1\ntext: hello\n---\nline: 2\ntext: world\n\n```\n"
        );
    }

    #[tokio::test]
    async fn no_rows_and_sql_errors_are_results_the_model_reads() {
        let r = execute(&context(), "SELECT * FROM message WHERE line > 5")
            .await
            .unwrap();
        assert_eq!(text_of(&r), "0 rows");
        let r = execute(&context(), "SELECT nope FROM message")
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).starts_with("SQL error: "));
    }

    #[test]
    fn wire_definition_comes_from_the_md() {
        let meta = build_tool_meta(MD);
        assert_eq!(meta.name, NAME);
        let annotations = meta.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.idempotent_hint, Some(true));
        let required = meta.input_schema.get("required").unwrap();
        assert_eq!(required, &json!(["sql"]));
    }
}
