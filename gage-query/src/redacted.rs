//! The `redacted` table shape: one row per record of a stored
//! session's `redacted.json`, the changes its publish made to the
//! original. A session that is not a publish has no rows.

use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;
use gage_store::RedactedRecord;

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("line_end", DataType::Int64, true),
        Field::new("property", DataType::Utf8, true),
        Field::new("span_start", DataType::Int64, true),
        Field::new("span_end", DataType::Int64, true),
        Field::new("reason", DataType::Utf8, false),
        Field::new("author", DataType::Utf8, false),
    ]))
});

pub fn redacted_schema() -> SchemaRef {
    SCHEMA.clone()
}

/// The `redacted` rows of one session, in record order.
pub fn redacted_batch(session_id: &str, records: &[RedactedRecord]) -> Result<RecordBatch> {
    let mut session_ids = StringBuilder::new();
    let mut kinds = StringBuilder::new();
    let mut lines = Int64Builder::new();
    let mut line_ends = Int64Builder::new();
    let mut properties = StringBuilder::new();
    let mut span_starts = Int64Builder::new();
    let mut span_ends = Int64Builder::new();
    let mut reasons = StringBuilder::new();
    let mut authors = StringBuilder::new();
    for r in records {
        session_ids.append_value(session_id);
        kinds.append_value(r.kind.as_str());
        lines.append_value(i64::from(r.line));
        line_ends.append_option(r.line_end.map(i64::from));
        properties.append_option(r.property.as_deref());
        span_starts.append_option(r.span_start.map(offset));
        span_ends.append_option(r.span_end.map(offset));
        reasons.append_value(&r.reason);
        authors.append_value(&r.author);
    }
    Ok(RecordBatch::try_new(
        redacted_schema(),
        vec![
            Arc::new(session_ids.finish()),
            Arc::new(kinds.finish()),
            Arc::new(lines.finish()),
            Arc::new(line_ends.finish()),
            Arc::new(properties.finish()),
            Arc::new(span_starts.finish()),
            Arc::new(span_ends.finish()),
            Arc::new(reasons.finish()),
            Arc::new(authors.finish()),
        ],
    )?)
}

/// A byte offset as the table's signed column holds it. An offset
/// beyond `i64::MAX` cannot occur in a text value Gage can read.
fn offset(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
