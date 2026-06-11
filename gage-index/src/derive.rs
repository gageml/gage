//! Derivation: session JSONL → derived rows and session aggregates.
//!
//! One pass over a session file produces a `RecordBatch` in the store
//! schema (a superset serving both the `entry` and `message` tables)
//! plus the session-level aggregates. JSONL is parsed only here;
//! every query path scans derived artifacts.

use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;

use arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::record_batch::RecordBatch;
use chrono::DateTime;
use gage_claude::entry::{entry_attachment_blocks, entry_subtype, entry_to_text, split_ide_tags};
use gage_claude::session_reader::SessionReader;
use serde::{Deserialize, Serialize};

use crate::Result;

// Store column indices. The store schema is a superset serving two
// tables: `entry` is a projection of every row; `message` is a filter
// (`type IN ('user','assistant') AND text IS NOT NULL`) plus the
// message-derived columns. `text` is non-null (possibly empty) exactly
// for the rows the `message` table contains.
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

static STORE_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
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
    ]))
});

pub fn store_schema() -> SchemaRef {
    STORE_SCHEMA.clone()
}

/// Source-file identity: the JSONL's `(mtime, size)` stat'd when
/// derivation opened it. Embedded in the session file's Parquet footer
/// and in the index manifest; equality means "absorbed".
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

/// Session-level aggregates: the expensive `session` table columns,
/// computed in the same derivation pass and consolidated into
/// `sessions.parquet`.
#[derive(Debug, Clone, Default)]
pub struct SessionAggregates {
    pub title: Option<String>,
    pub model: Option<String>,
    pub message_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub is_empty: bool,
}

pub struct DerivedSession {
    pub session_id: String,
    pub batch: RecordBatch,
    pub aggregates: SessionAggregates,
    pub fingerprint: Fingerprint,
}

/// Maximum number of characters of source text to include when
/// deriving a title from the first usable user message.
const MAX_TITLE_LEN_FROM_MSG: usize = 60;

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{}...", truncated.trim_end())
}

/// Derive a session title from a user entry, used as a fallback when
/// no `ai-title` entry is present. Returns `None` when the entry has
/// no usable text (e.g. purely tag-wrapped meta content).
fn session_title_from_entry(entry: &serde_json::Value) -> Option<String> {
    let text = entry_to_text(entry);
    let (body, _ide_tags) = split_ide_tags(&text);
    let line_end = body.find('\n').unwrap_or(body.len());
    let line = body.get(..line_end)?.trim();
    if line.is_empty() {
        return None;
    }
    Some(truncate_with_ellipsis(line, MAX_TITLE_LEN_FROM_MSG))
}

/// Whether a session entry carries real conversational content, used
/// to decide whether a session is empty. An assistant turn always
/// counts. A user turn counts only when its message is more than
/// out-of-band tags. Non-message entries never count.
fn entry_has_content(entry: &serde_json::Value) -> bool {
    match entry.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => true,
        Some("user") => match entry.get("message").and_then(|m| m.get("content")) {
            Some(serde_json::Value::String(s)) => !split_ide_tags(s).0.trim().is_empty(),
            Some(serde_json::Value::Array(blocks)) => !blocks.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

/// Extract the text representation of a raw session entry.
///
/// Extracts text from `message.content` blocks (text, thinking,
/// tool_use, tool_result) and joins them with `\n\n`. Returns `None`
/// for non-message entries or messages with no text content.
///
/// The returned text may contain leading IDE tags — callers that need
/// them separated should pass the result through `split_ide_tags`.
pub fn entry_text(entry: &serde_json::Value) -> Option<String> {
    let type_str = match entry.get("type").and_then(|v| v.as_str()) {
        Some(t @ ("user" | "assistant")) => t,
        _ => return None,
    };

    let content = match entry.get("message").and_then(|m| m.get("content")) {
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        Some(serde_json::Value::String(s)) => {
            vec![serde_json::json!({"type": "text", "text": s})]
        }
        _ => return None,
    };

    let mut texts: Vec<String> = Vec::new();

    for block in &content {
        let block_type = match block.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };

        match (type_str, block_type) {
            (_, "text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    texts.push(t.to_string());
                }
            }
            ("assistant", "thinking") => {
                if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                    texts.push(t.to_string());
                }
            }
            ("assistant", "tool_use") => {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block
                    .get("input")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                texts.push(format_tool_call_text(name, &input));
            }
            ("user", "tool_result") => {
                texts.push(tool_result_text(block));
            }
            _ => {}
        }
    }

    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

fn format_tool_call_text(name: &str, input: &serde_json::Map<String, serde_json::Value>) -> String {
    // Build a YAML mapping: tool name as key, arguments as nested map
    let mut mapping = serde_yaml::Mapping::new();
    let args_mapping: serde_yaml::Mapping = input
        .iter()
        .map(|(k, v)| (serde_yaml::Value::String(k.clone()), json_to_yaml(v)))
        .collect();
    mapping.insert(
        serde_yaml::Value::String(name.to_string()),
        serde_yaml::Value::Mapping(args_mapping),
    );
    let yaml = serde_yaml::to_string(&mapping).unwrap_or_default();
    // serde_yaml adds a trailing newline; trim it for the header portion
    yaml.trim_end().to_string()
}

fn json_to_yaml(v: &serde_json::Value) -> serde_yaml::Value {
    match v {
        serde_json::Value::Null => serde_yaml::Value::Null,
        serde_json::Value::Bool(b) => serde_yaml::Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_yaml::Value::Number(i.into())
            } else if let Some(f) = n.as_f64() {
                serde_yaml::Value::Number(serde_yaml::Number::from(f))
            } else {
                serde_yaml::Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => serde_yaml::Value::String(s.clone()),
        serde_json::Value::Array(arr) => {
            serde_yaml::Value::Sequence(arr.iter().map(json_to_yaml).collect())
        }
        serde_json::Value::Object(map) => {
            let m: serde_yaml::Mapping = map
                .iter()
                .map(|(k, v)| (serde_yaml::Value::String(k.clone()), json_to_yaml(v)))
                .collect();
            serde_yaml::Value::Mapping(m)
        }
    }
}

/// Extract tool_result text from a content block's `content` field,
/// which may be a string or an array of content items.
fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(t);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

struct RowBuilders {
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
}

impl RowBuilders {
    fn new() -> Self {
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
        }
    }

    fn finish(mut self) -> Result<RecordBatch> {
        Ok(RecordBatch::try_new(
            store_schema(),
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
            ],
        )?)
    }
}

/// Whether an entry qualifies as a `message` table row: a user or
/// assistant entry whose `message.content` is an array or a string.
/// Malformed content shapes are entry rows but not message rows.
fn is_message_row(entry: &serde_json::Value) -> bool {
    matches!(
        entry.get("type").and_then(|v| v.as_str()),
        Some("user" | "assistant")
    ) && matches!(
        entry.get("message").and_then(|m| m.get("content")),
        Some(serde_json::Value::Array(_)) | Some(serde_json::Value::String(_))
    )
}

/// Parse one session file and derive its store rows and aggregates.
///
/// Every parseable JSONL line becomes a row. The fingerprint is
/// stat'd before reading; a write racing the parse leaves a recorded
/// fingerprint older than the absorbed content, so the next reconcile
/// re-derives — staleness is self-healing, never wrong rows.
pub fn derive_session(session_id: &str, path: &Path) -> Result<DerivedSession> {
    let fingerprint = Fingerprint::stat(path)?;
    let reader = SessionReader::open(path)?;

    let mut b = RowBuilders::new();
    let mut agg = SessionAggregates {
        is_empty: true,
        ..Default::default()
    };

    for result in reader {
        let (line_num, entry) = match result {
            Ok(pair) => pair,
            Err(_) => continue,
        };

        let entry_type = entry.get("type").and_then(|v| v.as_str());
        let entry_uuid = entry.get("uuid").and_then(|v| v.as_str());
        let ts_ms = match entry.get("timestamp").and_then(|v| v.as_str()) {
            Some(s) => match DateTime::parse_from_rfc3339(s) {
                Ok(dt) => Some(dt.timestamp_millis()),
                Err(e) => {
                    tracing::warn!(
                        session_id,
                        line = line_num,
                        timestamp = s,
                        "unparseable entry timestamp: {e}"
                    );
                    None
                }
            },
            None => None,
        };

        // Aggregates (mirrors the previous session-table scan)
        if agg.is_empty && entry_has_content(&entry) {
            agg.is_empty = false;
        }
        match entry_type.unwrap_or("") {
            "user" | "assistant" => {
                agg.message_count += 1;
                if entry_type == Some("assistant") {
                    let msg = entry.get("message");
                    if agg.model.is_none() {
                        agg.model = msg
                            .and_then(|m| m.get("model"))
                            .and_then(|m| m.as_str())
                            .map(String::from);
                    }
                    if let Some(usage) = msg.and_then(|m| m.get("usage")) {
                        agg.input_tokens += usage
                            .get("input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        agg.output_tokens += usage
                            .get("output_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        agg.cache_read_input_tokens += usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        agg.cache_creation_input_tokens += usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                    }
                } else if entry_type == Some("user") && agg.title.is_none() {
                    agg.title = session_title_from_entry(&entry);
                }
            }
            "ai-title" => {
                agg.title = entry
                    .get("aiTitle")
                    .and_then(|t| t.as_str())
                    .map(String::from);
            }
            _ => {}
        }

        // Store row
        b.session_ids.append_value(session_id);
        b.lines.append_value(line_num as i64);
        match entry_uuid {
            Some(v) => b.uuids.append_value(v),
            None => b.uuids.append_null(),
        }
        match entry_type {
            Some(v) => b.types.append_value(v),
            None => b.types.append_null(),
        }
        b.timestamps.append_option(ts_ms);
        b.raws.append_value(entry.to_string());

        if is_message_row(&entry) {
            match entry_subtype(&entry) {
                Some(v) => b.subtypes.append_value(v),
                None => b.subtypes.append_null(),
            }

            let mut attachments: Vec<serde_json::Value> = Vec::new();
            for block in entry_attachment_blocks(&entry) {
                let content_index = entry
                    .pointer("/message/content")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.iter().position(|b| std::ptr::eq(b, block)))
                    .unwrap_or(0);
                let mut att = block.clone();
                if let Some(obj) = att.as_object_mut() {
                    obj.insert(
                        "ref".to_string(),
                        serde_json::json!([line_num, content_index]),
                    );
                }
                attachments.push(att);
            }

            let joined = entry_text(&entry).unwrap_or_default();
            let (text, ide_tags) = split_ide_tags(&joined);

            b.texts.append_value(&text);
            match attachments.is_empty() {
                true => b.attachments.append_null(),
                false => b
                    .attachments
                    .append_value(serde_json::Value::Array(attachments).to_string()),
            }
            match &ide_tags {
                Some(v) => b.ide_tags.append_value(v),
                None => b.ide_tags.append_null(),
            }
        } else {
            b.subtypes.append_null();
            b.texts.append_null();
            b.attachments.append_null();
            b.ide_tags.append_null();
        }
    }

    Ok(DerivedSession {
        session_id: session_id.to_string(),
        batch: b.finish()?,
        aggregates: agg,
        fingerprint,
    })
}
