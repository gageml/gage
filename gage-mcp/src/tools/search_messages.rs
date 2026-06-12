use std::future::Future;
use std::pin::Pin;

use arrow::array::{
    Array, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    LargeStringArray, StringArray, StringViewArray, UInt8Array, UInt16Array, UInt32Array,
    UInt64Array,
};
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};
use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::ToolRoute,
    handler::server::tool::ToolCallContext,
    model::{CallToolResult, Content},
};

use crate::server::GageServer;
use crate::tool::{MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/SearchMessages.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "SearchMessages description exceeds Claude Code's 2048-byte cap",
);

const DEFAULT_LIMIT: i64 = 20;

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
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::invalid_params("missing or non-string `query`", None))?;
        let limit = match params.get("limit") {
            None => DEFAULT_LIMIT,
            Some(v) => match v.as_i64() {
                Some(n) if n > 0 => n,
                _ => {
                    return Err(McpError::invalid_params(
                        "`limit` must be a positive integer",
                        None,
                    ));
                }
            },
        };
        let snippet_len = match params.get("snippet_len") {
            None => None,
            Some(v) => match v.as_i64() {
                Some(n) if n > 0 => Some(n),
                _ => {
                    return Err(McpError::invalid_params(
                        "`snippet_len` must be a positive integer",
                        None,
                    ));
                }
            },
        };

        let sql = build_sql(query, limit, snippet_len);
        let session_ctx = ctx.service.ctx().await;
        let df = match session_ctx.sql(&sql).await {
            Ok(df) => df,
            Err(e) => return Ok(domain_error(format!("query error: {e}"))),
        };
        let batches = match df.collect().await {
            Ok(b) => b,
            Err(e) => return Ok(domain_error(format!("search error: {e}"))),
        };

        let mut buf: Vec<u8> = Vec::new();
        if let Err(e) = write_yaml(&mut buf, &batches) {
            return Err(McpError::internal_error(format!("yaml encode: {e}"), None));
        }
        let text = String::from_utf8(buf)
            .map_err(|e| McpError::internal_error(format!("UTF-8 error: {e}"), None))?;
        Ok(success(text))
    })
}

/// Build the SQL invocation of the `message_text` TVF. The query
/// string is single-quote-escaped to keep parser-special characters
/// inside the string literal.
fn build_sql(query: &str, limit: i64, snippet_len: Option<i64>) -> String {
    let q = sql_escape(query);
    match snippet_len {
        Some(n) => format!(
            "SELECT session_id, line, score, snippet \
             FROM message_text('{q}', {n}) LIMIT {limit}"
        ),
        None => format!(
            "SELECT session_id, line, score, snippet \
             FROM message_text('{q}') LIMIT {limit}"
        ),
    }
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn success(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn domain_error(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

/// Emit `batches` as multi-document YAML — one document per row, `---`
/// separator before each. Field order follows the schema. Escaping is
/// handled by `serde_yaml`.
fn write_yaml<W: std::io::Write>(
    w: &mut W,
    batches: &[RecordBatch],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let format_opts = FormatOptions::default();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let formatters: Vec<ArrayFormatter> = batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &format_opts))
            .collect::<Result<_, _>>()?;
        let schema = batch.schema();
        for row in 0..batch.num_rows() {
            writeln!(w, "---")?;
            let mut mapping = serde_yaml::Mapping::with_capacity(schema.fields().len());
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let col = batch.column(col_idx);
                let fmt = formatters
                    .get(col_idx)
                    .expect("formatters correspond to batch schema fields");
                let value = cell_to_yaml(col.as_ref(), row, fmt);
                mapping.insert(serde_yaml::Value::String(field.name().clone()), value);
            }
            serde_yaml::to_writer(&mut *w, &serde_yaml::Value::Mapping(mapping))?;
        }
    }
    Ok(())
}

fn cell_to_yaml(col: &dyn Array, row: usize, fallback: &ArrayFormatter) -> serde_yaml::Value {
    use serde_yaml::Value;
    if col.is_null(row) {
        return Value::Null;
    }
    match col.data_type() {
        DataType::Utf8 => Value::String(
            col.as_any()
                .downcast_ref::<StringArray>()
                .expect("Utf8 → StringArray")
                .value(row)
                .to_string(),
        ),
        DataType::LargeUtf8 => Value::String(
            col.as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("LargeUtf8 → LargeStringArray")
                .value(row)
                .to_string(),
        ),
        DataType::Utf8View => Value::String(
            col.as_any()
                .downcast_ref::<StringViewArray>()
                .expect("Utf8View → StringViewArray")
                .value(row)
                .to_string(),
        ),
        DataType::Boolean => Value::Bool(
            col.as_any()
                .downcast_ref::<BooleanArray>()
                .expect("Boolean → BooleanArray")
                .value(row),
        ),
        DataType::Int8 => {
            i64_value(col.as_any().downcast_ref::<Int8Array>().unwrap().value(row) as i64)
        }
        DataType::Int16 => i64_value(
            col.as_any()
                .downcast_ref::<Int16Array>()
                .unwrap()
                .value(row) as i64,
        ),
        DataType::Int32 => i64_value(
            col.as_any()
                .downcast_ref::<Int32Array>()
                .unwrap()
                .value(row) as i64,
        ),
        DataType::Int64 => i64_value(
            col.as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(row),
        ),
        DataType::UInt8 => u64_value(
            col.as_any()
                .downcast_ref::<UInt8Array>()
                .unwrap()
                .value(row) as u64,
        ),
        DataType::UInt16 => u64_value(
            col.as_any()
                .downcast_ref::<UInt16Array>()
                .unwrap()
                .value(row) as u64,
        ),
        DataType::UInt32 => u64_value(
            col.as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap()
                .value(row) as u64,
        ),
        DataType::UInt64 => u64_value(
            col.as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
                .value(row),
        ),
        DataType::Float32 => f64_value(
            col.as_any()
                .downcast_ref::<Float32Array>()
                .unwrap()
                .value(row) as f64,
        ),
        DataType::Float64 => f64_value(
            col.as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(row),
        ),
        _ => Value::String(fallback.value(row).to_string()),
    }
}

fn i64_value(n: i64) -> serde_yaml::Value {
    serde_yaml::Value::Number(serde_yaml::Number::from(n))
}

fn u64_value(n: u64) -> serde_yaml::Value {
    serde_yaml::Value::Number(serde_yaml::Number::from(n))
}

fn f64_value(n: f64) -> serde_yaml::Value {
    serde_yaml::Value::Number(serde_yaml::Number::from(n))
}
