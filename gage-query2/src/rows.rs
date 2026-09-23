//! Row shapes for the `entry` and `message` tables and the batch
//! built from a driver's normalized entries.
//!
//! One derived batch per stored session version serves both tables:
//! `entry` is a projection of every row; `message` is the rows whose
//! `text` is non-null, with the message columns projected. The batch
//! is built once per commit and held in [`RowCache`] for the life of
//! the query context. A commit's bytes never change, so a cached
//! batch is never stale.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{
    Array, BooleanArray, Int64Builder, StringArray, StringBuilder, TimestampMillisecondBuilder,
};
use datafusion::arrow::compute::filter_record_batch;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use gage_session::{DriverError, Entry};

const COL_SESSION_ID: usize = 0;
const COL_LINE: usize = 1;
const COL_UUID: usize = 2;
const COL_TYPE: usize = 3;
const COL_SUBTYPE: usize = 4;
const COL_TIMESTAMP: usize = 5;
const COL_RAW: usize = 6;
const COL_TEXT: usize = 7;
const COL_ATTACHMENTS: usize = 8;
const COL_IDE_TAGS: usize = 9;
const COL_MESSAGE_SUBTYPE: usize = 10;

/// Derived columns serving `entry`, in table order
const ENTRY_PROJECTION: &[usize] = &[
    COL_SESSION_ID,
    COL_LINE,
    COL_UUID,
    COL_TYPE,
    COL_SUBTYPE,
    COL_TIMESTAMP,
    COL_RAW,
];

/// Derived columns serving `message`, in table order. The derived
/// `message_subtype` is exposed as `subtype`.
const MESSAGE_PROJECTION: &[usize] = &[
    COL_SESSION_ID,
    COL_LINE,
    COL_UUID,
    COL_TYPE,
    COL_MESSAGE_SUBTYPE,
    COL_TEXT,
    COL_TIMESTAMP,
    COL_ATTACHMENTS,
    COL_IDE_TAGS,
    COL_RAW,
];

/// Index into [`MESSAGE_PROJECTION`] of the renamed subtype column
const MESSAGE_SUBTYPE_POS: usize = 4;

fn derived_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("uuid", DataType::Utf8, true),
        Field::new("type", DataType::Utf8, true),
        Field::new("subtype", DataType::Utf8, true),
        Field::new(
            "timestamp",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new("raw", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, true),
        Field::new("attachments", DataType::Utf8, true),
        Field::new("ide_tags", DataType::Utf8, true),
        Field::new("message_subtype", DataType::Utf8, true),
    ]))
}

pub fn entry_schema() -> SchemaRef {
    Arc::new(
        derived_schema()
            .project(ENTRY_PROJECTION)
            .expect("entry projection indexes the derived schema"),
    )
}

pub fn message_schema() -> SchemaRef {
    let projected = derived_schema()
        .project(MESSAGE_PROJECTION)
        .expect("message projection indexes the derived schema");
    let fields: Vec<Field> = projected
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i == MESSAGE_SUBTYPE_POS {
                Field::new("subtype", f.data_type().clone(), f.is_nullable())
            } else {
                f.as_ref().clone()
            }
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// The derived batch of one session from the driver's entries.
/// `session_id` is the value written to every row's `session_id`.
pub fn derive_batch(
    session_id: &str,
    entries: impl Iterator<Item = Result<Entry, DriverError>>,
) -> Result<RecordBatch> {
    let mut session_ids = StringBuilder::new();
    let mut lines = Int64Builder::new();
    let mut uuids = StringBuilder::new();
    let mut types = StringBuilder::new();
    let mut subtypes = StringBuilder::new();
    let mut timestamps = TimestampMillisecondBuilder::new();
    let mut raws = StringBuilder::new();
    let mut texts = StringBuilder::new();
    let mut attachments = StringBuilder::new();
    let mut ide_tags = StringBuilder::new();
    let mut message_subtypes = StringBuilder::new();

    for entry in entries {
        let e = entry.map_err(|e| DataFusionError::External(Box::new(e)))?;
        session_ids.append_value(session_id);
        lines.append_value(e.line as i64);
        uuids.append_option(e.uuid.as_deref());
        types.append_option(e.entry_type.as_deref());
        subtypes.append_option(e.subtype.as_deref());
        timestamps.append_option(e.timestamp_ms);
        raws.append_value(&e.raw);
        let m = e.message.as_ref();
        texts.append_option(m.map(|m| m.text.as_str()));
        attachments.append_option(m.and_then(|m| m.attachments.as_deref()));
        ide_tags.append_option(m.and_then(|m| m.ide_tags.as_deref()));
        message_subtypes.append_option(m.and_then(|m| m.subtype.as_deref()));
    }

    Ok(RecordBatch::try_new(
        derived_schema(),
        vec![
            Arc::new(session_ids.finish()),
            Arc::new(lines.finish()),
            Arc::new(uuids.finish()),
            Arc::new(types.finish()),
            Arc::new(subtypes.finish()),
            Arc::new(timestamps.finish().with_timezone("UTC")),
            Arc::new(raws.finish()),
            Arc::new(texts.finish()),
            Arc::new(attachments.finish()),
            Arc::new(ide_tags.finish()),
            Arc::new(message_subtypes.finish()),
        ],
    )?)
}

/// A derived batch in `entry` table shape
pub fn entry_rows(derived: &RecordBatch) -> Result<RecordBatch> {
    Ok(derived.project(ENTRY_PROJECTION)?)
}

/// The message rows of a derived batch in `message` table shape
pub fn message_rows(derived: &RecordBatch) -> Result<RecordBatch> {
    let texts = derived
        .column(COL_TEXT)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DataFusionError::Internal("derived text column type".into()))?;
    let mask: BooleanArray = (0..derived.num_rows())
        .map(|i| Some(texts.is_valid(i)))
        .collect();
    let projected = filter_record_batch(derived, &mask)?.project(MESSAGE_PROJECTION)?;
    // The projected schema still names the subtype column
    // `message_subtype`; rebind the columns to the table schema
    Ok(RecordBatch::try_new(
        message_schema(),
        projected.columns().to_vec(),
    )?)
}

/// Derived batches by commit sha, shared by every `entry` and
/// `message` scan on one query context.
#[derive(Debug, Default)]
pub struct RowCache {
    map: Mutex<HashMap<String, Arc<RecordBatch>>>,
}

impl RowCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, commit: &str) -> Option<Arc<RecordBatch>> {
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(commit)
            .cloned()
    }

    pub fn insert(&self, commit: &str, batch: Arc<RecordBatch>) {
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(commit.to_string(), batch);
    }
}
