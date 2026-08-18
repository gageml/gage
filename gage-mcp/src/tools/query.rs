use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use gage_query::slow_log;
use gage_query::write_yaml_capped;
use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::ToolRoute,
    handler::server::tool::ToolCallContext,
    model::{CallToolResult, Content},
};
use serde_json::json;

use crate::server::GageServer;
use crate::tool::{MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/Query.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "QueryGage description exceeds Claude Code's 2048-byte cap",
);

/// Maximum serialized byte size for one `query` result page. A result
/// over this is returned as a truncated first page with continuation
/// instructions, keeping paging in SQL where the model can act on it.
/// Claude Code persists any tool result over 50,000 characters to a
/// file the model sees only a 2KB preview of (v2.1.51+), so pages must
/// stay under that; 45,000 bytes leaves margin for the truncation
/// banner and threshold drift (a UTF-8 byte count never undercounts
/// characters).
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

        let session_ctx = ctx.service.ctx().await;
        let start = Instant::now();
        let df = match session_ctx.sql(sql).await {
            Ok(df) => df,
            Err(e) => {
                let msg = format!("SQL error: {e}");
                slow_log::record(sql, start.elapsed(), None, Some(&msg));
                return Ok(domain_error(msg));
            }
        };
        let batches = match df.collect().await {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("query execution error: {e}");
                slow_log::record(sql, start.elapsed(), None, Some(&msg));
                return Ok(domain_error(msg));
            }
        };
        let batches: Vec<_> = batches
            .iter()
            .filter(|b| b.num_rows() > 0)
            .cloned()
            .collect();
        let row_count: usize = batches.iter().map(|b| b.num_rows()).sum();
        slow_log::record(sql, start.elapsed(), Some(row_count), None);
        if batches.is_empty() {
            return Ok(success("0 rows"));
        }
        let mut buf: Vec<u8> = b"```yaml\n".to_vec();
        let rows_written = match write_yaml_capped(&mut buf, &batches, PAGE_CAP_BYTES) {
            Ok(n) => n,
            Err(e) => return Ok(domain_error(format!("YAML serialization error: {e}"))),
        };
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
    })
}

fn success(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn domain_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}
