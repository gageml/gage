//! The `filtered` table shape: one row per record of a stored
//! session's `filtered.json`, the rules its publish applied to the
//! original. A session that is not a publish has no rows.

use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;
use gage_store::FilteredRecord;

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("line_end", DataType::Int64, true),
        Field::new("ref", DataType::Utf8, true),
        Field::new("span_start", DataType::Int64, true),
        Field::new("span_end", DataType::Int64, true),
        Field::new("reason", DataType::Utf8, false),
        Field::new("author", DataType::Utf8, false),
    ]))
});

pub fn filtered_schema() -> SchemaRef {
    SCHEMA.clone()
}

/// The `filtered` rows of one session, in record order.
pub fn filtered_batch(session_id: &str, records: &[FilteredRecord]) -> Result<RecordBatch> {
    let mut session_ids = StringBuilder::new();
    let mut kinds = StringBuilder::new();
    let mut lines = Int64Builder::new();
    let mut line_ends = Int64Builder::new();
    let mut references = StringBuilder::new();
    let mut span_starts = Int64Builder::new();
    let mut span_ends = Int64Builder::new();
    let mut reasons = StringBuilder::new();
    let mut authors = StringBuilder::new();
    for r in records {
        session_ids.append_value(session_id);
        kinds.append_value(r.kind.as_str());
        lines.append_value(i64::from(r.line));
        line_ends.append_option(r.line_end.map(i64::from));
        references.append_option(r.reference.as_deref());
        span_starts.append_option(r.span_start.map(offset));
        span_ends.append_option(r.span_end.map(offset));
        reasons.append_value(&r.reason);
        authors.append_value(&r.author);
    }
    Ok(RecordBatch::try_new(
        filtered_schema(),
        vec![
            Arc::new(session_ids.finish()),
            Arc::new(kinds.finish()),
            Arc::new(lines.finish()),
            Arc::new(line_ends.finish()),
            Arc::new(references.finish()),
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
