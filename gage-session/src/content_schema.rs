//! The `content_schema` table: a driver's description of the native
//! content it writes into the session tables.
//!
//! The entry, message, and tool_use tables are the contract every
//! driver fills. Their `raw`, `input_raw`, and `result_raw` columns
//! hold the driver's native record verbatim, so their shape is the
//! driver's to state. A driver states it per content format it reads,
//! keyed by the `content_format` value the session table carries, as
//! a JSON Schema. A reader scopes rows to one content format before
//! reading into a native column.

use std::sync::Arc;

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;

/// The native field a content schema describes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentField {
    /// `entry.raw` and `message.raw`: one transcript record
    Entry,
    /// `tool_use.input_raw`: the arguments of one tool call
    ToolUseInput,
    /// `tool_use.result_raw`: the result of one tool call
    ToolUseResult,
}

impl ContentField {
    /// The `name` column value
    pub fn as_str(self) -> &'static str {
        match self {
            ContentField::Entry => "entry",
            ContentField::ToolUseInput => "tool_use_input",
            ContentField::ToolUseResult => "tool_use_result",
        }
    }
}

/// One `content_schema` row
#[derive(Debug, Clone)]
pub struct ContentSchema {
    /// The `content_format` the session table records for sessions in
    /// this shape
    pub content_format: String,
    pub field: ContentField,
    /// A JSON Schema document, complete or partial
    pub schema: String,
}

pub fn content_schema_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("content_format", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("schema", DataType::Utf8, false),
    ]))
}

/// The `content_schema` batch for `rows`
pub fn content_schema_batch(rows: &[ContentSchema]) -> Result<RecordBatch> {
    let mut formats = StringBuilder::new();
    let mut names = StringBuilder::new();
    let mut schemas = StringBuilder::new();
    for row in rows {
        formats.append_value(&row.content_format);
        names.append_value(row.field.as_str());
        schemas.append_value(&row.schema);
    }
    Ok(RecordBatch::try_new(
        content_schema_schema(),
        vec![
            Arc::new(formats.finish()),
            Arc::new(names.finish()),
            Arc::new(schemas.finish()),
        ],
    )?)
}
