//! What one pass over a session file derives: a `RecordBatch` in the
//! derived schema (a superset serving both the `entry` and `message`
//! tables), the session's `tool_use` and `tool_use_file` rows, and the
//! session-level attributes. A driver's [`crate::Corpus::derive`]
//! produces it; the in-memory cache and the summary cache hold the
//! results.

use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::record_batch::RecordBatch;
use gage_session::{Entry, SessionAttrs, ToolUse, ToolUseBatch, ToolUseFile, ToolUseFileBatch};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Result;

// Derived column indices. The derived schema is a superset serving
// two tables: `entry` is a projection of every row; `message` is a
// filter (`text IS NOT NULL`) plus the message-derived columns.
// `text` is non-null (possibly empty) exactly for the rows the
// `message` table contains.
pub const COL_SESSION_ID: usize = 0;
pub const COL_LINE: usize = 1;
pub const COL_UUID: usize = 2;
pub const COL_TYPE: usize = 3;
pub const COL_SUBTYPE: usize = 4;
pub const COL_TIMESTAMP: usize = 5;
pub const COL_RAW: usize = 6;
pub const COL_TEXT: usize = 7;
pub const COL_ATTACHMENTS: usize = 8;
pub const COL_IDE_TAGS: usize = 9;
pub const COL_MESSAGE_SUBTYPE: usize = 10;
pub const COL_MESSAGE_TYPE: usize = 11;

static DERIVED_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
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
        Field::new("message_type", DataType::Utf8, true),
    ]))
});

pub fn derived_schema() -> SchemaRef {
    DERIVED_SCHEMA.clone()
}

/// Source-file identity: the session file's `(mtime, size)` stat'd
/// when derivation opened it. Recorded in the in-memory cache and in
/// the index manifest; equality means "absorbed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub mtime_ms: i64,
    pub size: u64,
}

impl Fingerprint {
    pub fn stat(path: &Path) -> std::io::Result<Self> {
        let meta = path.metadata()?;
        let mtime_ms = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Ok(Self {
            mtime_ms,
            size: meta.len(),
        })
    }
}

/// The attributes of one native session: the `session` table columns
/// and the facts the store records when the session is added, computed
/// in one derivation pass and cached per session file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorpusSessionAttrs {
    /// The session file's mtime at derivation, epoch milliseconds
    #[serde(default)]
    pub native_mtime_ms: i64,
    /// The session file's size at derivation
    #[serde(default)]
    pub native_size: u64,
    /// The project name, as the driver's listing gives it unless a
    /// caller recorded another
    #[serde(default)]
    pub project: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub message_count: i64,
    /// Prompt tokens neither read from nor written to a prompt cache.
    /// The session's `input_tokens` is this plus the two cache counts.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    /// The session's cost as the harness reported it, when it reports
    /// one
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub is_empty: bool,
    /// The line number of the last parsed entry; a trailing line the
    /// reader could not parse is not counted
    #[serde(default)]
    pub line_count: u64,
    /// The earliest entry timestamp, epoch milliseconds
    #[serde(default)]
    pub first_timestamp_ms: Option<i64>,
    /// The latest entry timestamp, epoch milliseconds
    #[serde(default)]
    pub last_timestamp_ms: Option<i64>,
    /// Every path a tool call names with what the record proves about
    /// the call and the path, each pair once, sorted: the session's
    /// `tool_use_file` rows without the call. The walk prunes on it.
    #[serde(default)]
    pub files: Vec<ToolUseFile>,
    /// Why the session file could not be derived, when it could not.
    /// A session with a read error has none of the derived values;
    /// the walk still lists it.
    #[serde(default)]
    pub read_error: Option<String>,
    /// The version of the harness's file format the session file
    /// records, for a harness whose files record one. A driver orders
    /// two states of a session by it when a later format can hold
    /// fewer lines.
    #[serde(default)]
    pub format_version: Option<u32>,
}

impl CorpusSessionAttrs {
    /// The attributes of a session file that failed to derive: the
    /// facts the walk supplies and the error, cached against
    /// `fingerprint` like a derived session so the file is read again
    /// only when it changes.
    pub fn with_read_error(
        project: impl Into<String>,
        fingerprint: Fingerprint,
        error: impl std::fmt::Display,
    ) -> Self {
        Self {
            native_mtime_ms: fingerprint.mtime_ms,
            native_size: fingerprint.size,
            project: project.into(),
            read_error: Some(error.to_string()),
            ..Default::default()
        }
    }

    /// A derived value, or `None` when the session has a read error
    fn derived<T>(&self, value: impl FnOnce(&Self) -> T) -> Option<T> {
        match self.read_error {
            Some(_) => None,
            None => Some(value(self)),
        }
    }
}

/// A session with a read error has no derived values: every derived
/// accessor is `None`, and `extra_attrs` carries the error.
impl SessionAttrs for CorpusSessionAttrs {
    fn native_mtime(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.native_mtime_ms.max(0) as u64)
    }

    fn native_size(&self) -> u64 {
        self.native_size
    }

    fn is_empty(&self) -> Option<bool> {
        self.derived(|a| a.is_empty)
    }

    fn project_name(&self) -> Option<&str> {
        Some(&self.project)
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn message_count(&self) -> Option<u64> {
        self.derived(|a| a.message_count.max(0) as u64)
    }

    fn line_count(&self) -> Option<u64> {
        self.derived(|a| a.line_count)
    }

    fn input_tokens(&self) -> Option<u64> {
        self.derived(|a| {
            let total = a.input_tokens + a.cache_read_input_tokens + a.cache_creation_input_tokens;
            total.max(0) as u64
        })
    }

    fn output_tokens(&self) -> Option<u64> {
        self.derived(|a| a.output_tokens.max(0) as u64)
    }

    fn cached_tokens(&self) -> Option<u64> {
        self.derived(|a| a.cache_read_input_tokens.max(0) as u64)
    }

    fn cost_usd(&self) -> Option<f64> {
        self.cost_usd
    }

    /// The token split the normalized totals fold together, the span
    /// of entry timestamps, the format version when the file records
    /// one, and the read error when there is one.
    fn extra_attrs(&self) -> Map<String, Value> {
        let mut extra = Map::new();
        if let Some(error) = &self.read_error {
            extra.insert("read_error".to_string(), Value::from(error.as_str()));
            return extra;
        }
        extra.insert(
            "cache_read_input_tokens".to_string(),
            Value::from(self.cache_read_input_tokens),
        );
        extra.insert(
            "cache_creation_input_tokens".to_string(),
            Value::from(self.cache_creation_input_tokens),
        );
        if let Some(ms) = self.first_timestamp_ms {
            extra.insert("first_timestamp_ms".to_string(), Value::from(ms));
        }
        if let Some(ms) = self.last_timestamp_ms {
            extra.insert("last_timestamp_ms".to_string(), Value::from(ms));
        }
        if let Some(version) = self.format_version {
            extra.insert("format_version".to_string(), Value::from(version));
        }
        extra
    }
}

pub struct DerivedSession {
    pub session_id: String,
    pub batch: RecordBatch,
    /// The session's `tool_use` rows
    pub tool_uses: RecordBatch,
    /// The session's `tool_use_file` rows.
    pub tool_use_files: RecordBatch,
    pub attrs: CorpusSessionAttrs,
    pub fingerprint: Fingerprint,
}

impl DerivedSession {
    /// The derived session of `rows` and `tool_uses` with `attrs`.
    /// The files of the tool uses become the attributes' `files`.
    pub fn new(
        session_id: &str,
        rows: DerivedRows,
        tool_uses: Vec<ToolUse>,
        mut attrs: CorpusSessionAttrs,
        fingerprint: Fingerprint,
    ) -> Result<Self> {
        let mut tool_use_batch = ToolUseBatch::new();
        let mut tool_use_file_batch = ToolUseFileBatch::new();
        for t in &tool_uses {
            tool_use_batch.push(session_id, t);
            tool_use_file_batch.push(session_id, t);
            attrs.files.extend(t.files.iter().cloned());
        }
        attrs.files.sort();
        attrs.files.dedup();
        Ok(Self {
            session_id: session_id.to_string(),
            batch: rows.finish()?,
            tool_uses: tool_use_batch.finish()?,
            tool_use_files: tool_use_file_batch.finish()?,
            attrs,
            fingerprint,
        })
    }
}

/// Accumulates [`Entry`] rows into a batch of the derived schema.
pub struct DerivedRows {
    session_ids: StringBuilder,
    lines: Int64Builder,
    uuids: StringBuilder,
    types: StringBuilder,
    subtypes: StringBuilder,
    timestamps: TimestampMillisecondBuilder,
    raws: StringBuilder,
    texts: StringBuilder,
    attachments: StringBuilder,
    ide_tags: StringBuilder,
    message_subtypes: StringBuilder,
    message_types: StringBuilder,
}

impl Default for DerivedRows {
    fn default() -> Self {
        Self::new()
    }
}

impl DerivedRows {
    pub fn new() -> Self {
        Self {
            session_ids: StringBuilder::new(),
            lines: Int64Builder::new(),
            uuids: StringBuilder::new(),
            types: StringBuilder::new(),
            subtypes: StringBuilder::new(),
            timestamps: TimestampMillisecondBuilder::new(),
            raws: StringBuilder::new(),
            texts: StringBuilder::new(),
            attachments: StringBuilder::new(),
            ide_tags: StringBuilder::new(),
            message_subtypes: StringBuilder::new(),
            message_types: StringBuilder::new(),
        }
    }

    pub fn push(&mut self, session_id: &str, e: &Entry) {
        self.session_ids.append_value(session_id);
        self.lines.append_value(e.line as i64);
        self.uuids.append_option(e.uuid.as_deref());
        self.types.append_value(&e.entry_type);
        self.subtypes.append_option(e.subtype.as_deref());
        self.timestamps.append_option(e.timestamp_ms);
        self.raws.append_option(e.raw.as_deref());
        let m = e.message.as_ref();
        self.texts.append_option(m.map(|m| m.text.as_str()));
        self.attachments
            .append_option(m.and_then(|m| m.attachments.as_deref()));
        self.ide_tags
            .append_option(m.and_then(|m| m.ide_tags.as_deref()));
        self.message_subtypes
            .append_option(m.and_then(|m| m.subtype.as_deref()));
        self.message_types
            .append_option(m.map(|m| m.message_type.as_str()));
    }

    pub fn finish(mut self) -> Result<RecordBatch> {
        Ok(RecordBatch::try_new(
            derived_schema(),
            vec![
                Arc::new(self.session_ids.finish()),
                Arc::new(self.lines.finish()),
                Arc::new(self.uuids.finish()),
                Arc::new(self.types.finish()),
                Arc::new(self.subtypes.finish()),
                Arc::new(self.timestamps.finish().with_timezone("UTC")),
                Arc::new(self.raws.finish()),
                Arc::new(self.texts.finish()),
                Arc::new(self.attachments.finish()),
                Arc::new(self.ide_tags.finish()),
                Arc::new(self.message_subtypes.finish()),
                Arc::new(self.message_types.finish()),
            ],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_version_is_an_extra_attribute_when_the_file_records_one() {
        let attrs = CorpusSessionAttrs::default();
        assert!(!attrs.extra_attrs().contains_key("format_version"));
        let attrs = CorpusSessionAttrs {
            format_version: Some(3),
            ..Default::default()
        };
        assert_eq!(attrs.extra_attrs()["format_version"], Value::from(3));
    }

    /// The summary cache holds the attributes as a MessagePack array
    /// in field order, so a file written before the last field existed
    /// is one element short.
    #[test]
    fn a_cached_summary_written_without_the_format_version_still_reads() {
        let attrs = CorpusSessionAttrs {
            project: "p".to_string(),
            line_count: 7,
            ..Default::default()
        };
        let mut bytes = rmp_serde::to_vec(&attrs).unwrap();
        assert_eq!(bytes[..3], [0xdc, 0, 18], "an array of the 18 fields");
        bytes[2] = 17;
        assert_eq!(bytes.pop(), Some(0xc0), "the format version, nil");
        let read: CorpusSessionAttrs = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(read.line_count, 7);
        assert_eq!(read.format_version, None);
    }
}
