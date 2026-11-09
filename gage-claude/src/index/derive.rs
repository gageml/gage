//! Derivation: session JSONL → derived rows and session aggregates.
//!
//! One pass over a session file produces a `RecordBatch` in the
//! derived schema (a superset serving both the `entry` and `message`
//! tables) plus the session-level aggregates. JSONL is parsed only
//! here; the in-memory cache in `gage-query` holds the results.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;

use crate::entry::{
    entry_attachment_blocks, entry_subtype, entry_to_text, message_subtype, split_ide_tags,
};
use crate::session_reader::SessionReader;
use arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::record_batch::RecordBatch;
use chrono::DateTime;
use gage_session::{Entry, Message, ToolUseBatch, ToolUseFile, ToolUseFileBatch};
use serde::{Deserialize, Serialize};

use super::Result;
use super::tool_use::ToolUseCollector;

// Derived column indices. The derived schema is a superset serving
// two tables: `entry` is a projection of every row; `message` is a
// filter (`type IN ('user','assistant') AND text IS NOT NULL`) plus
// the message-derived columns. `text` is non-null (possibly empty)
// exactly for the rows the `message` table contains.
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
    ]))
});

pub fn derived_schema() -> SchemaRef {
    DERIVED_SCHEMA.clone()
}

/// Source-file identity: the JSONL's `(mtime, size)` stat'd when
/// derivation opened it. Recorded in the in-memory cache and in the
/// index manifest; equality means "absorbed".
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
/// in one derivation pass and cached per transcript file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeSessionAttrs {
    /// The transcript file's mtime at derivation, epoch milliseconds
    #[serde(default)]
    pub native_mtime_ms: i64,
    /// The transcript file's size at derivation
    #[serde(default)]
    pub native_size: u64,
    /// The project slug: the name of the directory holding the
    /// transcript, unless a caller recorded another
    #[serde(default)]
    pub project: String,
    pub title: Option<String>,
    /// The model of the first assistant row the API produced, else the
    /// model the first API request named. Claude Code writes the rows
    /// it generates itself, such as API error text, with the model
    /// `<synthetic>`; those never name the model.
    pub model: Option<String>,
    pub message_count: i64,
    /// The token totals of the last `cost-state` record when the
    /// session has one, else the sum over the assistant rows with each
    /// API response counted once. A refused request leaves no assistant
    /// row but is counted in `cost-state`, as are subagent runs and the
    /// title model; the totals then match `cost_usd`.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    /// The running total of the last `cost-state` record, which Claude
    /// Code writes for sessions started from its CLI
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
    /// Why the transcript could not be derived, when it could not. A
    /// session with a read error has none of the derived values; the
    /// walk still lists it.
    #[serde(default)]
    pub read_error: Option<String>,
}

impl ClaudeSessionAttrs {
    /// The attributes of a transcript that failed to derive: the facts
    /// the walk supplies and the error, cached against `fingerprint`
    /// like a derived session so the file is read again only when it
    /// changes.
    pub fn with_read_error(
        path: &Path,
        fingerprint: Fingerprint,
        error: impl fmt::Display,
    ) -> Self {
        Self {
            native_mtime_ms: fingerprint.mtime_ms,
            native_size: fingerprint.size,
            project: project_of(path),
            read_error: Some(error.to_string()),
            ..Default::default()
        }
    }
}

/// The project slug of a transcript: the name of the directory
/// holding it.
pub fn project_of(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub struct DerivedSession {
    pub session_id: String,
    pub batch: RecordBatch,
    /// The session's `tool_use` rows
    pub tool_uses: RecordBatch,
    /// The session's `tool_use_file` rows.
    pub tool_use_files: RecordBatch,
    pub attrs: ClaudeSessionAttrs,
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
/// no `ai-title` entry is present: the first non-empty line of the
/// body. Returns `None` when the entry has no usable text (e.g. purely
/// tag-wrapped meta content, or a text block emptied by a publish).
fn session_title_from_entry(entry: &serde_json::Value) -> Option<String> {
    let text = entry_to_text(entry);
    let (body, _ide_tags) = split_ide_tags(&text);
    let line = body.lines().map(str::trim).find(|line| !line.is_empty())?;
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
/// tool_use, tool_result) and joins them with `\n\n`. System entries
/// with a promoted subtype render per `system_entry_text`. Returns
/// `None` for non-message entries or messages with no text content.
///
/// The returned text may contain leading IDE tags — callers that need
/// them separated should pass the result through `split_ide_tags`.
pub fn entry_text(entry: &serde_json::Value) -> Option<String> {
    let type_str = match entry.get("type").and_then(|v| v.as_str()) {
        Some(t @ ("user" | "assistant")) => t,
        Some("system") => return system_entry_text(entry),
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

/// Text for a promoted system entry, hard-coded per subtype. The
/// content-bearing subtypes (`compact_boundary`, `away_summary`,
/// `informational`, `local_command`) use their `content` string
/// verbatim — tags included, as tool results do. `api_error` has no
/// `content`; it renders its error and retry fields as a YAML mapping,
/// the same shape as a tool call. A refusal's `content` is empty or a
/// model-switch notice; its text is the API's `apiRefusalExplanation`.
/// Non-promoted subtypes return `None`.
fn system_entry_text(entry: &serde_json::Value) -> Option<String> {
    match message_subtype(entry)? {
        "model_refusal" => entry
            .get("apiRefusalExplanation")
            .and_then(|v| v.as_str())
            .map(String::from),
        "api_error" => {
            let mut fields = serde_json::Map::new();
            for key in ["error", "retryAttempt", "retryInMs", "maxRetries"] {
                if let Some(v) = entry.get(key) {
                    fields.insert(key.to_string(), v.clone());
                }
            }
            Some(format_tool_call_text("api_error", &fields))
        }
        _ => entry
            .get("content")
            .and_then(|v| v.as_str())
            .map(String::from),
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
pub(crate) fn tool_result_text(block: &serde_json::Value) -> String {
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
    message_subtypes: StringBuilder,
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
            message_subtypes: StringBuilder::new(),
        }
    }

    fn push(&mut self, session_id: &str, e: &Entry) {
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
    }

    fn finish(mut self) -> Result<RecordBatch> {
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
            ],
        )?)
    }
}

/// Whether an entry qualifies as a `message` table row: a user or
/// assistant entry whose `message.content` is an array or a string,
/// or a system entry whose subtype is promoted to the message flow
/// (see `SYSTEM_MESSAGE_SUBTYPES` in gage-claude). Malformed content
/// shapes are entry rows but not message rows.
pub fn is_message_row(entry: &serde_json::Value) -> bool {
    match entry.get("type").and_then(|v| v.as_str()) {
        Some("user" | "assistant") => matches!(
            entry.get("message").and_then(|m| m.get("content")),
            Some(serde_json::Value::Array(_)) | Some(serde_json::Value::String(_))
        ),
        Some("system") => message_subtype(entry).is_some(),
        _ => false,
    }
}

/// Parse one session file and derive its rows and aggregates.
///
/// Every parseable JSONL line becomes a row. The fingerprint is
/// stat'd before reading; a write racing the parse leaves a recorded
/// fingerprint older than the absorbed content, so the next reconcile
/// re-derives — staleness is self-healing, never wrong rows.
pub fn derive_session(session_id: &str, path: &Path) -> Result<DerivedSession> {
    let fingerprint = Fingerprint::stat(path)?;
    derive_session_from(
        session_id,
        SessionReader::open(path)?,
        fingerprint,
        project_of(path),
    )
}

/// Derive the rows and aggregates of the transcript `reader` yields.
/// `fingerprint` and `project` are the facts the content itself does
/// not carry; [`derive_session`] takes them from the file.
pub fn derive_session_from<R: std::io::Read>(
    session_id: &str,
    reader: SessionReader<R>,
    fingerprint: Fingerprint,
    project: String,
) -> Result<DerivedSession> {
    let mut b = RowBuilders::new();
    let mut tool_uses = ToolUseCollector::new();
    let mut attrs = ClaudeSessionAttrs {
        native_mtime_ms: fingerprint.mtime_ms,
        native_size: fingerprint.size,
        project,
        is_empty: true,
        ..Default::default()
    };
    let mut has_custom_title = false;
    let mut counted_responses: HashSet<String> = HashSet::new();
    let mut requested_model: Option<String> = None;
    let mut cost_state_usage: Option<TokenUsage> = None;

    for result in reader {
        // A line that does not read or decode fails the session. The
        // codec stands in for the corruption it recognizes with an
        // invalid entry, which derives a row like any other line.
        let (line_num, raw, decoded) = result?;
        let entry = serde_json::Value::from(decoded);

        let entry_type = entry.get("type").and_then(|v| v.as_str());
        attrs.line_count = u64::from(line_num);
        if let Some(ms) = entry_timestamp_ms(&entry) {
            attrs.first_timestamp_ms = Some(attrs.first_timestamp_ms.map_or(ms, |f| f.min(ms)));
            attrs.last_timestamp_ms = Some(attrs.last_timestamp_ms.map_or(ms, |l| l.max(ms)));
        }

        if attrs.is_empty && entry_has_content(&entry) {
            attrs.is_empty = false;
        }
        match entry_type.unwrap_or("") {
            "user" | "assistant" => {
                attrs.message_count += 1;
                if entry_type == Some("assistant") {
                    let msg = entry.get("message");
                    if attrs.model.is_none() {
                        attrs.model = msg
                            .and_then(|m| m.get("model"))
                            .and_then(|m| m.as_str())
                            .filter(|m| *m != SYNTHETIC_MODEL)
                            .map(String::from);
                    }
                    // One API response is written as one row per content
                    // block, each repeating the response's usage
                    let response_id = msg.and_then(|m| m.get("id")).and_then(|v| v.as_str());
                    let first_row_of_response = match response_id {
                        Some(id) => counted_responses.insert(id.to_string()),
                        None => true,
                    };
                    if let Some(usage) = msg.and_then(|m| m.get("usage"))
                        && first_row_of_response
                    {
                        attrs.input_tokens += usage
                            .get("input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        attrs.output_tokens += usage
                            .get("output_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        attrs.cache_read_input_tokens += usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        attrs.cache_creation_input_tokens += usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                    }
                } else if entry_type == Some("user") && attrs.title.is_none() {
                    attrs.title = session_title_from_entry(&entry);
                }
            }
            "ai-title" => {
                if !has_custom_title {
                    attrs.title = entry
                        .get("aiTitle")
                        .and_then(|t| t.as_str())
                        .map(String::from);
                }
            }
            "custom-title" => {
                if let Some(t) = entry.get("customTitle").and_then(|t| t.as_str()) {
                    attrs.title = Some(t.to_string());
                    has_custom_title = true;
                }
            }
            "api-request" => {
                if requested_model.is_none() {
                    requested_model = entry
                        .pointer("/params/model")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                }
            }
            "cost-state" => {
                if let Some(cost) = entry.get("totalCostUSD").and_then(|v| v.as_f64()) {
                    attrs.cost_usd = Some(cost);
                }
                if let Some(usage) = cost_state_token_usage(&entry) {
                    cost_state_usage = Some(usage);
                }
            }
            _ => {}
        }

        if let Some(row) = derive_entry(session_id, line_num, &entry, raw) {
            b.push(session_id, &row);
        }
        tool_uses.push(line_num, &entry);
    }

    let mut tool_use_batch = ToolUseBatch::new();
    let mut tool_use_file_batch = ToolUseFileBatch::new();
    for t in tool_uses.finish() {
        tool_use_batch.push(session_id, &t);
        tool_use_file_batch.push(session_id, &t);
        attrs.files.extend(t.files.iter().cloned());
    }
    attrs.files.sort();
    attrs.files.dedup();
    if attrs.model.is_none() {
        attrs.model = requested_model;
    }
    if let Some(usage) = cost_state_usage {
        attrs.input_tokens = usage.input;
        attrs.output_tokens = usage.output;
        attrs.cache_read_input_tokens = usage.cache_read;
        attrs.cache_creation_input_tokens = usage.cache_creation;
    }

    Ok(DerivedSession {
        session_id: session_id.to_string(),
        batch: b.finish()?,
        tool_uses: tool_use_batch.finish()?,
        tool_use_files: tool_use_file_batch.finish()?,
        attrs,
        fingerprint,
    })
}

/// The model Claude Code writes on an assistant row it generated
/// itself rather than received from the API
const SYNTHETIC_MODEL: &str = "<synthetic>";

struct TokenUsage {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
}

/// The token totals of a `cost-state` entry summed over its
/// `modelUsage` map; `None` when the entry has no such map.
fn cost_state_token_usage(entry: &serde_json::Value) -> Option<TokenUsage> {
    let models = entry.get("modelUsage")?.as_object()?;
    let mut usage = TokenUsage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_creation: 0,
    };
    for model in models.values() {
        let count = |k| model.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        usage.input += count("inputTokens");
        usage.output += count("outputTokens");
        usage.cache_read += count("cacheReadInputTokens");
        usage.cache_creation += count("cacheCreationInputTokens");
    }
    Some(usage)
}

/// The entry's `timestamp` as epoch milliseconds; `None` when absent
/// or not RFC 3339.
pub(crate) fn entry_timestamp_ms(entry: &serde_json::Value) -> Option<i64> {
    let s = entry.get("timestamp").and_then(|v| v.as_str())?;
    Some(DateTime::parse_from_rfc3339(s).ok()?.timestamp_millis())
}

/// Normalize one parsed session line into `entry` table shape. This
/// is the row half of [`derive_session`]; the stored-session reader
/// calls it per line without the session aggregates. `session_id`
/// labels diagnostics only. A line without a `type` is not an entry
/// and yields `None`, as an unparseable line yields no row.
/// `raw` is the entry's text as the codec renders it, the `raw` column.
pub fn derive_entry(
    session_id: &str,
    line_num: u32,
    entry: &serde_json::Value,
    raw: String,
) -> Option<Entry> {
    let Some(entry_type) = entry.get("type").and_then(|v| v.as_str()) else {
        tracing::warn!(session_id, line = line_num, "entry without a type");
        return None;
    };
    let timestamp_ms = match entry.get("timestamp").and_then(|v| v.as_str()) {
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

    let message = is_message_row(entry).then(|| {
        let msg_subtype = message_subtype(entry);

        let mut attachments: Vec<serde_json::Value> = Vec::new();
        for block in entry_attachment_blocks(entry) {
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

        let joined = entry_text(entry).unwrap_or_default();
        // Out-of-band tags are prepended to user prompts only. A
        // tool result, a compaction summary, or an assistant turn that
        // opens with a tag-shaped pair (e.g. <tool_use_error>) is
        // content.
        let (text, ide_tags) = match (entry_type, msg_subtype) {
            ("user", Some("text" | "meta")) => split_ide_tags(&joined),
            _ => (joined, None),
        };
        Message {
            message_type: entry_type.to_string(),
            subtype: msg_subtype.map(String::from),
            text,
            attachments: (!attachments.is_empty())
                .then(|| serde_json::Value::Array(attachments).to_string()),
            ide_tags,
        }
    });

    Some(Entry {
        line: line_num,
        uuid: entry.get("uuid").and_then(|v| v.as_str()).map(String::from),
        entry_type: entry_type.to_string(),
        subtype: entry_subtype(entry).map(String::from),
        timestamp_ms,
        raw: Some(raw),
        message,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use arrow::array::{Array, StringArray};
    use gage_session::FileType;
    use serde_json::json;

    /// The attrs carry the distinct `(path, type)` pairs of the
    /// session's calls, and the `tool_use_file` batch carries one row
    /// per call per path.
    #[test]
    fn summary_carries_the_time_span_and_the_files_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            json!({"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:05Z",
                   "message":{"role":"user","content":"go"}}),
            json!({"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","cwd":"/w",
                   "message":{"role":"assistant","content":[
                       {"type":"tool_use","id":"t1","name":"Write",
                        "input":{"file_path":"/w/b.rs","content":"x"}},
                       {"type":"tool_use","id":"t2","name":"Write",
                        "input":{"file_path":"/w/a.rs","content":"x"}},
                       {"type":"tool_use","id":"t3","name":"Bash",
                        "input":{"command":"cat > ~/c.json <<EOF\n{}\nEOF\ncat /w/a.rs d/e.rs"}}]}}),
            json!({"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:09Z",
                   "message":{"role":"user","content":[
                       {"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
            json!({"type":"user","uuid":"u3","timestamp":"2025-01-01T00:00:09Z",
                   "message":{"role":"user","content":[
                       {"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}),
            json!({"type":"summary","summary":"s","leafUuid":"a1"}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let derived = derive_session("s", &path).unwrap();
        assert_eq!(derived.attrs.first_timestamp_ms, Some(1_735_689_601_000));
        assert_eq!(derived.attrs.last_timestamp_ms, Some(1_735_689_609_000));
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let file = |path: PathBuf, type_: FileType| ToolUseFile { path, type_ };
        let mut expected = vec![
            file(home.join("c.json"), FileType::Cite),
            file(PathBuf::from("/w/a.rs"), FileType::Write),
            file(PathBuf::from("/w/a.rs"), FileType::Cite),
            file(PathBuf::from("/w/b.rs"), FileType::Write),
            file(PathBuf::from("/w/d/e.rs"), FileType::Cite),
        ];
        expected.sort();
        assert_eq!(
            derived.attrs.files, expected,
            "a path a judged call wrote and an unjudged call named is two pairs"
        );
        assert_eq!(derived.tool_uses.num_rows(), 3);
        assert_eq!(derived.tool_use_files.num_rows(), 5);
    }

    #[test]
    fn token_usage_counts_each_api_response_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let usage = json!({"input_tokens":3,"output_tokens":10,
                           "cache_read_input_tokens":100,"cache_creation_input_tokens":7});
        let lines = [
            json!({"type":"assistant","uuid":"a1","message":{"id":"msg_1","role":"assistant",
                   "content":[{"type":"text","text":"one"}],"usage":usage}}),
            json!({"type":"assistant","uuid":"a2","message":{"id":"msg_1","role":"assistant",
                   "content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}],"usage":usage}}),
            json!({"type":"assistant","uuid":"a3","message":{"id":"msg_2","role":"assistant",
                   "content":[{"type":"text","text":"two"}],"usage":usage}}),
            json!({"type":"assistant","uuid":"a4","message":{"role":"assistant",
                   "content":[{"type":"text","text":"no id"}],"usage":usage}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let attrs = derive_session("s", &path).unwrap().attrs;
        assert_eq!(attrs.message_count, 4);
        assert_eq!(attrs.input_tokens, 9);
        assert_eq!(attrs.output_tokens, 30);
        assert_eq!(attrs.cache_read_input_tokens, 300);
        assert_eq!(attrs.cache_creation_input_tokens, 21);
    }

    /// Without an `ai-title` entry the title is the first non-empty
    /// line of the first user message. A publish that removes a
    /// leading tag block leaves an empty first text block, so the
    /// text opens with blank lines.
    #[test]
    fn title_falls_back_to_the_first_non_empty_line_of_the_first_user_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            json!({"type":"user","uuid":"u1","message":{"role":"user","content":[
                {"type":"text","text":""},
                {"type":"text","text":"\nCompare entire.com with Gage\nin detail"}]}}),
            json!({"type":"user","uuid":"u2","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"Launching skill: x"}]}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let attrs = derive_session("s", &path).unwrap().attrs;
        assert_eq!(attrs.title.as_deref(), Some("Compare entire.com with Gage"));
    }

    #[test]
    fn cost_is_the_last_cost_state_total() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            json!({"type":"user","uuid":"u1","message":{"role":"user","content":"go"}}),
            json!({"type":"cost-state","sessionId":"s","totalCostUSD":0.5}),
            json!({"type":"cost-state","sessionId":"s","totalCostUSD":1.25}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let attrs = derive_session("s", &path).unwrap().attrs;
        assert_eq!(attrs.cost_usd, Some(1.25));
        let no_cost = dir.path().join("t.jsonl");
        std::fs::write(&no_cost, format!("{}\n", lines[0])).unwrap();
        assert_eq!(derive_session("t", &no_cost).unwrap().attrs.cost_usd, None);
    }

    fn write_lines(dir: &Path, name: &str, lines: &[serde_json::Value]) -> PathBuf {
        let path = dir.join(name);
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn assistant_row(model: &str, id: &str) -> serde_json::Value {
        json!({"type":"assistant","uuid":id,"message":{"id":id,"role":"assistant","model":model,
               "content":[{"type":"text","text":"t"}]}})
    }

    /// A refused request leaves only a `<synthetic>` assistant row,
    /// which never names the model; the request that was refused does.
    #[test]
    fn model_skips_synthetic_rows_and_falls_back_to_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let request = json!({"type":"api-request","sessionId":"s",
                             "params":{"model":"claude-fable-5-1","max_tokens":1}});
        let refused = write_lines(
            dir.path(),
            "refused.jsonl",
            &[request.clone(), assistant_row("<synthetic>", "e1")],
        );
        let attrs = derive_session("s", &refused).unwrap().attrs;
        assert_eq!(attrs.model.as_deref(), Some("claude-fable-5-1"));

        let answered = write_lines(
            dir.path(),
            "answered.jsonl",
            &[
                request,
                assistant_row("<synthetic>", "e1"),
                assistant_row("claude-x", "m1"),
            ],
        );
        let attrs = derive_session("s", &answered).unwrap().attrs;
        assert_eq!(attrs.model.as_deref(), Some("claude-x"));

        let no_request = write_lines(
            dir.path(),
            "none.jsonl",
            &[assistant_row("<synthetic>", "e1")],
        );
        assert_eq!(derive_session("s", &no_request).unwrap().attrs.model, None);
    }

    /// The harness's `modelUsage` counts what the assistant rows cannot:
    /// refused requests, subagent runs, and the title model. Its totals
    /// replace the row sums so the tokens match `cost_usd`.
    #[test]
    fn token_totals_prefer_the_cost_state_model_usage() {
        let dir = tempfile::tempdir().unwrap();
        let usage = json!({"input_tokens":3,"output_tokens":10,
                           "cache_read_input_tokens":100,"cache_creation_input_tokens":7});
        let path = write_lines(
            dir.path(),
            "s.jsonl",
            &[
                json!({"type":"assistant","uuid":"a1","message":{"id":"msg_1","role":"assistant",
                       "content":[{"type":"text","text":"one"}],"usage":usage}}),
                json!({"type":"cost-state","sessionId":"s","totalCostUSD":0.1,"modelUsage":{
                    "claude-fable-5-1":{"inputTokens":4,"outputTokens":0,
                        "cacheReadInputTokens":29123,"cacheCreationInputTokens":45882},
                    "claude-opus-4-7":{"inputTokens":1199,"outputTokens":19,
                        "cacheReadInputTokens":0,"cacheCreationInputTokens":0}}}),
            ],
        );
        let attrs = derive_session("s", &path).unwrap().attrs;
        assert_eq!(attrs.input_tokens, 1203);
        assert_eq!(attrs.output_tokens, 19);
        assert_eq!(attrs.cache_read_input_tokens, 29123);
        assert_eq!(attrs.cache_creation_input_tokens, 45882);
        assert_eq!(attrs.cost_usd, Some(0.1));
    }

    fn system_entry(subtype: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut entry = json!({ "type": "system", "subtype": subtype });
        entry
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        entry
    }

    #[test]
    fn promoted_system_subtypes_are_message_rows() {
        for subtype in [
            "api_error",
            "away_summary",
            "compact_boundary",
            "informational",
            "local_command",
            "model_refusal_no_fallback",
            "model_refusal_fallback",
        ] {
            let entry = system_entry(subtype, json!({}));
            assert!(is_message_row(&entry), "{subtype} should be a message row");
        }
    }

    #[test]
    fn both_refusal_forms_are_one_subtype_with_the_api_explanation_as_text() {
        for subtype in ["model_refusal_no_fallback", "model_refusal_fallback"] {
            let entry = system_entry(
                subtype,
                json!({ "content": "Switched model", "apiRefusalExplanation": "Why" }),
            );
            assert_eq!(message_subtype(&entry), Some("model_refusal"), "{subtype}");
            assert_eq!(entry_text(&entry).as_deref(), Some("Why"), "{subtype}");
        }
    }

    #[test]
    fn compaction_summary_is_its_own_subtype_with_content_verbatim() {
        let summary = "<system-reminder>x</system-reminder>\nThis session is being continued";
        let entry = json!({
            "type": "user",
            "isCompactSummary": true,
            "isVisibleInTranscriptOnly": true,
            "message": { "role": "user", "content": summary },
        });
        assert_eq!(message_subtype(&entry), Some("compact_summary"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, format!("{entry}\n")).unwrap();
        let batch = derive_session("s", &path).unwrap().batch;
        let column = |i: usize| {
            batch
                .column(i)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
        };
        assert_eq!(column(COL_MESSAGE_SUBTYPE).value(0), "compact_summary");
        assert_eq!(column(COL_TEXT).value(0), summary);
        assert!(column(COL_IDE_TAGS).is_null(0), "no tag split on a summary");
    }

    #[test]
    fn bookkeeping_system_subtypes_stay_entry_only() {
        for subtype in ["turn_duration", "stop_hook_summary", "never_heard_of_it"] {
            let entry = system_entry(subtype, json!({ "content": "x" }));
            assert!(!is_message_row(&entry), "{subtype} should stay entry-only");
            assert_eq!(entry_text(&entry), None);
        }
    }

    #[test]
    fn system_content_subtypes_use_content_verbatim() {
        let compact = system_entry(
            "compact_boundary",
            json!({ "content": "Conversation compacted" }),
        );
        assert_eq!(
            entry_text(&compact).as_deref(),
            Some("Conversation compacted")
        );

        // Wrapper tags are content, as they are for tool results
        let stdout = "<local-command-stdout>## Context Usage</local-command-stdout>";
        let cmd = system_entry("local_command", json!({ "content": stdout }));
        assert_eq!(entry_text(&cmd).as_deref(), Some(stdout));
    }

    #[test]
    fn api_error_text_renders_like_tool_call() {
        let entry = system_entry(
            "api_error",
            json!({
                "error": { "formatted": "529 Overloaded", "status": 529 },
                "retryAttempt": 1,
                "maxRetries": 10,
            }),
        );
        let text = entry_text(&entry).unwrap();
        assert!(text.starts_with("api_error:"), "yaml mapping key: {text}");
        assert!(text.contains("formatted: 529 Overloaded"), "{text}");
        assert!(text.contains("retryAttempt: 1"), "{text}");
        assert!(text.contains("maxRetries: 10"), "{text}");
    }

    /// A torn line, one entry cut short with the next entry's bytes
    /// on the same line, is a row of the invalid entry type whose raw
    /// column holds the line; the lines around it derive as usual.
    #[test]
    fn a_torn_line_is_a_row_of_the_invalid_entry_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let torn = r#"{"type":"user","uuid":"u2","message":{"role":"user","content":"Exit co{"type":"last-prompt","lastPrompt":"go"}"#;
        std::fs::write(
            &path,
            format!(
                "{}\n{torn}\n{}\n",
                json!({"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:05Z",
                       "message":{"role":"user","content":"go"}}),
                json!({"type":"summary","summary":"s","leafUuid":"u1"}),
            ),
        )
        .unwrap();
        let derived = derive_session("s", &path).unwrap();
        assert_eq!(derived.batch.num_rows(), 3);
        assert_eq!(derived.attrs.line_count, 3);
        let column = |col: usize| {
            derived
                .batch
                .column(col)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(1)
                .to_string()
        };
        assert_eq!(column(COL_TYPE), "$gage-invalid-entry");
        let raw: serde_json::Value = serde_json::from_str(&column(COL_RAW)).unwrap();
        assert_eq!(raw["raw"], torn);
        assert!(
            raw["error"]
                .as_str()
                .is_some_and(|e| e.contains("expected")),
            "{raw}"
        );
    }

    /// A line the codec does not recognize fails the derivation, and
    /// the error names the line.
    #[test]
    fn a_line_the_codec_does_not_recognize_fails_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"type\":\"summary\"}\nnot an entry\n").unwrap();
        let err = match derive_session("s", &path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a line the codec does not recognize was derived"),
        };
        assert!(err.starts_with("line 2: "), "{err}");
    }
}
