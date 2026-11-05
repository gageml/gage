//! The `tool_use_file` table shape: one row per tool invocation per
//! path it names, with how the record relates the call to the path.
//! A driver yields the files on each [`ToolUse`] row; [`ToolUseFileBatch`]
//! builds the table's batch from them, so every driver and both the
//! native and stored paths produce one schema.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::error::ArrowError;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use serde::{Deserialize, Serialize};

use crate::{DriverError, ToolUse};

/// What the record proves about a call and a path it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FileType {
    /// The record proves the call wrote the path.
    Write,
    /// The record proves the call did not write the path.
    Read,
    /// The call names the path and the record does not say whether
    /// the call wrote it.
    Cite,
}

impl FileType {
    pub const ALL: [FileType; 3] = [FileType::Write, FileType::Read, FileType::Cite];

    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Write => "write",
            FileType::Read => "read",
            FileType::Cite => "cite",
        }
    }
}

impl fmt::Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FileType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        FileType::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| format!("unknown file type {s:?}"))
    }
}

/// One path a tool invocation names, with what the record proves
/// about the call and the path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolUseFile {
    /// The path as the record names it, absolute and normalized.
    pub path: PathBuf,
    pub type_: FileType,
}

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("tool_use_id", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
    ]))
});

pub fn tool_use_file_schema() -> SchemaRef {
    SCHEMA.clone()
}

/// Accumulates the files of [`ToolUse`] rows into the table's batch.
pub struct ToolUseFileBatch {
    session_ids: StringBuilder,
    tool_use_ids: StringBuilder,
    paths: StringBuilder,
    types: StringBuilder,
}

impl Default for ToolUseFileBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolUseFileBatch {
    pub fn new() -> Self {
        Self {
            session_ids: StringBuilder::new(),
            tool_use_ids: StringBuilder::new(),
            paths: StringBuilder::new(),
            types: StringBuilder::new(),
        }
    }

    /// Append one row per file of `t`, under `session_id`.
    pub fn push(&mut self, session_id: &str, t: &ToolUse) {
        for file in &t.files {
            self.session_ids.append_value(session_id);
            self.tool_use_ids.append_value(&t.id);
            self.paths.append_value(file.path.to_string_lossy());
            self.types.append_value(file.type_.as_str());
        }
    }

    pub fn finish(mut self) -> std::result::Result<RecordBatch, ArrowError> {
        RecordBatch::try_new(
            tool_use_file_schema(),
            vec![
                Arc::new(self.session_ids.finish()),
                Arc::new(self.tool_use_ids.finish()),
                Arc::new(self.paths.finish()),
                Arc::new(self.types.finish()),
            ],
        )
    }
}

/// The `tool_use_file` batch of one session from a driver's rows.
/// `session_id` is written to every row.
pub fn tool_use_file_batch(
    session_id: &str,
    rows: impl IntoIterator<Item = std::result::Result<ToolUse, DriverError>>,
) -> Result<RecordBatch> {
    let mut batch = ToolUseFileBatch::new();
    for row in rows {
        let t = row.map_err(|e| DataFusionError::External(Box::new(e)))?;
        batch.push(session_id, &t);
    }
    Ok(batch.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::StringArray;

    fn call(id: &str, files: Vec<ToolUseFile>) -> ToolUse {
        ToolUse {
            id: id.to_string(),
            name: "Bash".to_string(),
            line: 3,
            uuid: None,
            timestamp_ms: None,
            result_line: None,
            result_uuid: None,
            result_timestamp_ms: None,
            output: None,
            is_error: None,
            input_raw: "{}".to_string(),
            result_raw: None,
            files,
        }
    }

    fn file(path: &str, type_: FileType) -> ToolUseFile {
        ToolUseFile {
            path: PathBuf::from(path),
            type_,
        }
    }

    fn column(batch: &RecordBatch, name: &str) -> Vec<String> {
        let col = batch
            .column_by_name(name)
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        col.iter().map(|v| v.unwrap().to_string()).collect()
    }

    #[test]
    fn batch_has_one_row_per_file_and_no_row_for_a_call_without_files() {
        let batch = tool_use_file_batch(
            "s1",
            [
                Ok(call(
                    "t1",
                    vec![
                        file("/w/a.rs", FileType::Write),
                        file("/w/b.rs", FileType::Read),
                    ],
                )),
                Ok(call("t2", vec![])),
                Ok(call("t3", vec![file("/w/c.rs", FileType::Cite)])),
            ],
        )
        .unwrap();
        assert_eq!(batch.schema(), tool_use_file_schema());
        assert_eq!(column(&batch, "session_id"), ["s1", "s1", "s1"]);
        assert_eq!(column(&batch, "tool_use_id"), ["t1", "t1", "t3"]);
        assert_eq!(column(&batch, "path"), ["/w/a.rs", "/w/b.rs", "/w/c.rs"]);
        assert_eq!(column(&batch, "type"), ["write", "read", "cite"]);
    }

    #[test]
    fn file_type_round_trips_through_its_name() {
        for t in FileType::ALL {
            assert_eq!(t.as_str().parse::<FileType>().unwrap(), t);
        }
        let err = "wrote".parse::<FileType>().unwrap_err();
        assert!(err.contains("wrote"), "{err}");
    }

    #[test]
    fn a_driver_error_fails_the_batch() {
        let err = tool_use_file_batch(
            "s1",
            [
                Ok(call("t1", vec![])),
                Err(DriverError::Other("bad line".into())),
            ],
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("bad line"), "{err}");
    }
}
