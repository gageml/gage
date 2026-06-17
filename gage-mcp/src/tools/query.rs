use std::future::Future;
use std::pin::Pin;

use gage_query::write_yaml;
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

/// Maximum serialized byte size for a `query` result. Above this we
/// return a structured size-cap error with remediation hints so the
/// model can narrow the query before the harness-level truncation
/// fires. Sized to stay comfortably under Claude Code's ~25k-token
/// tool-result cap (≈100 KB of JSON).
const RESULT_CAP_BYTES: usize = 60_000;

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
        let df = match session_ctx.sql(sql).await {
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
        if batches.is_empty() {
            return Ok(success(""));
        }
        let row_count: usize = batches.iter().map(|b| b.num_rows()).sum();
        let mut buf: Vec<u8> = b"```yaml\n".to_vec();
        if let Err(e) = write_yaml(&mut buf, &batches) {
            return Ok(domain_error(format!("YAML serialization error: {e}")));
        }
        buf.extend_from_slice(b"\n```\n");
        if buf.len() > RESULT_CAP_BYTES {
            let msg = json!({
                "error": "result exceeds size cap",
                "bytes": buf.len(),
                "cap_bytes": RESULT_CAP_BYTES,
                "rows": row_count,
                "suggestion": "Re-run with a smaller LIMIT, paginate with `line > N`, \
                               SELECT substr(raw, 1, 800) instead of raw, \
                               or omit the raw column entirely.",
            })
            .to_string();
            return Ok(domain_error(msg));
        }
        let text = String::from_utf8(buf)
            .map_err(|e| McpError::internal_error(format!("UTF-8 error: {e}"), None))?;
        Ok(success(text))
    })
}

fn success(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn domain_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}
