//! The `tool_use` table shape: one row per tool invocation, pairing
//! the entry that made the call with the entry that returned its
//! result. A driver yields [`ToolUse`] rows from its native record;
//! [`ToolUseBatch`] builds the table's batch from them, so every
//! driver and both the native and stored paths produce one schema.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{
    BooleanBuilder, Int64Builder, ListBuilder, StringBuilder, TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::error::ArrowError;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};

use crate::DriverError;

/// One tool invocation of a session. The call side is always present;
/// the result side is `None` throughout when the record holds no
/// result for the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUse {
    /// The invocation id the record carries; the key that pairs a
    /// call with its result
    pub id: String,
    /// The tool name as the harness records it
    pub name: String,
    /// 1-based line of the entry that made the call
    pub line: u32,
    pub uuid: Option<String>,
    /// Epoch milliseconds, UTC
    pub timestamp_ms: Option<i64>,
    /// 1-based line of the entry that returned the result
    pub result_line: Option<u32>,
    pub result_uuid: Option<String>,
    pub result_timestamp_ms: Option<i64>,
    /// The result content as text
    pub output: Option<String>,
    pub is_error: Option<bool>,
    /// The call input, JSON as the harness recorded it
    pub input_raw: String,
    /// The harness's result record for the call, JSON as recorded
    pub result_raw: Option<String>,
    /// The paths the call wrote, as the driver determines from its
    /// record. `Some(vec![])` is a call that wrote nothing; `None` is
    /// a call the driver cannot judge.
    pub files_changed: Option<Vec<PathBuf>>,
}

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    let ts = || DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()));
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("uuid", DataType::Utf8, true),
        Field::new("timestamp", ts(), true),
        Field::new("result_line", DataType::Int64, true),
        Field::new("result_uuid", DataType::Utf8, true),
        Field::new("result_timestamp", ts(), true),
        Field::new("output", DataType::Utf8, true),
        Field::new("is_error", DataType::Boolean, true),
        Field::new("input_raw", DataType::Utf8, false),
        Field::new("result_raw", DataType::Utf8, true),
        Field::new(
            "files_changed",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
    ]))
});

pub fn tool_use_schema() -> SchemaRef {
    SCHEMA.clone()
}

/// Accumulates [`ToolUse`] rows into the table's batch.
pub struct ToolUseBatch {
    session_ids: StringBuilder,
    ids: StringBuilder,
    names: StringBuilder,
    lines: Int64Builder,
    uuids: StringBuilder,
    timestamps: TimestampMillisecondBuilder,
    result_lines: Int64Builder,
    result_uuids: StringBuilder,
    result_timestamps: TimestampMillisecondBuilder,
    outputs: StringBuilder,
    is_errors: BooleanBuilder,
    input_raws: StringBuilder,
    result_raws: StringBuilder,
    files_changed: ListBuilder<StringBuilder>,
}

impl Default for ToolUseBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolUseBatch {
    pub fn new() -> Self {
        Self {
            session_ids: StringBuilder::new(),
            ids: StringBuilder::new(),
            names: StringBuilder::new(),
            lines: Int64Builder::new(),
            uuids: StringBuilder::new(),
            timestamps: TimestampMillisecondBuilder::new(),
            result_lines: Int64Builder::new(),
            result_uuids: StringBuilder::new(),
            result_timestamps: TimestampMillisecondBuilder::new(),
            outputs: StringBuilder::new(),
            is_errors: BooleanBuilder::new(),
            input_raws: StringBuilder::new(),
            result_raws: StringBuilder::new(),
            files_changed: ListBuilder::new(StringBuilder::new()),
        }
    }

    /// Append one row under `session_id`.
    pub fn push(&mut self, session_id: &str, t: &ToolUse) {
        self.session_ids.append_value(session_id);
        self.ids.append_value(&t.id);
        self.names.append_value(&t.name);
        self.lines.append_value(i64::from(t.line));
        self.uuids.append_option(t.uuid.as_deref());
        self.timestamps.append_option(t.timestamp_ms);
        self.result_lines
            .append_option(t.result_line.map(i64::from));
        self.result_uuids.append_option(t.result_uuid.as_deref());
        self.result_timestamps.append_option(t.result_timestamp_ms);
        self.outputs.append_option(t.output.as_deref());
        self.is_errors.append_option(t.is_error);
        self.input_raws.append_value(&t.input_raw);
        self.result_raws.append_option(t.result_raw.as_deref());
        match &t.files_changed {
            Some(paths) => {
                for p in paths {
                    self.files_changed
                        .values()
                        .append_value(p.to_string_lossy());
                }
                self.files_changed.append(true);
            }
            None => self.files_changed.append(false),
        }
    }

    pub fn finish(mut self) -> std::result::Result<RecordBatch, ArrowError> {
        RecordBatch::try_new(
            tool_use_schema(),
            vec![
                Arc::new(self.session_ids.finish()),
                Arc::new(self.ids.finish()),
                Arc::new(self.names.finish()),
                Arc::new(self.lines.finish()),
                Arc::new(self.uuids.finish()),
                Arc::new(self.timestamps.finish().with_timezone("UTC")),
                Arc::new(self.result_lines.finish()),
                Arc::new(self.result_uuids.finish()),
                Arc::new(self.result_timestamps.finish().with_timezone("UTC")),
                Arc::new(self.outputs.finish()),
                Arc::new(self.is_errors.finish()),
                Arc::new(self.input_raws.finish()),
                Arc::new(self.result_raws.finish()),
                Arc::new(self.files_changed.finish()),
            ],
        )
    }
}

/// The `tool_use` batch of one session from a driver's rows.
/// `session_id` is written to every row.
pub fn tool_use_batch(
    session_id: &str,
    rows: impl IntoIterator<Item = std::result::Result<ToolUse, DriverError>>,
) -> Result<RecordBatch> {
    let mut batch = ToolUseBatch::new();
    for row in rows {
        let t = row.map_err(|e| DataFusionError::External(Box::new(e)))?;
        batch.push(session_id, &t);
    }
    Ok(batch.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{Array, ListArray, StringArray};

    fn call(id: &str) -> ToolUse {
        ToolUse {
            id: id.to_string(),
            name: "Edit".to_string(),
            line: 3,
            uuid: Some("a1".to_string()),
            timestamp_ms: Some(1_000),
            result_line: None,
            result_uuid: None,
            result_timestamp_ms: None,
            output: None,
            is_error: None,
            input_raw: "{}".to_string(),
            result_raw: None,
            files_changed: None,
        }
    }

    #[test]
    fn batch_matches_the_schema_and_keeps_list_nullability() {
        let answered = ToolUse {
            result_line: Some(4),
            result_uuid: Some("u1".to_string()),
            result_timestamp_ms: Some(2_000),
            output: Some("ok".to_string()),
            is_error: Some(false),
            result_raw: Some("{\"filePath\":\"/w/a.rs\"}".to_string()),
            files_changed: Some(vec![PathBuf::from("/w/a.rs")]),
            ..call("t1")
        };
        let wrote_nothing = ToolUse {
            files_changed: Some(vec![]),
            ..call("t2")
        };
        let unknown = call("t3");

        let batch = tool_use_batch("s1", [Ok(answered), Ok(wrote_nothing), Ok(unknown)]).unwrap();
        assert_eq!(batch.schema(), tool_use_schema());
        assert_eq!(batch.num_rows(), 3);

        let files = batch
            .column_by_name("files_changed")
            .unwrap()
            .as_any()
            .downcast_ref::<ListArray>()
            .unwrap();
        let first = files.value(0);
        let first = first.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(first.value(0), "/w/a.rs");
        assert_eq!(files.value(1).len(), 0);
        assert!(files.is_valid(1), "an empty list is a judged call");
        assert!(files.is_null(2), "null is an unjudged call");
    }

    #[test]
    fn a_driver_error_fails_the_batch() {
        let err = tool_use_batch(
            "s1",
            [Ok(call("t1")), Err(DriverError::Other("bad line".into()))],
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("bad line"), "{err}");
    }
}
