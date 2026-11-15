//! Derivation: a rollout file to derived rows and session attributes.
//!
//! A rollout line is `{timestamp, ordinal, type, payload}`. The
//! entry's type is the line's `type` and its subtype is the payload's
//! `type`, both as Codex names them. The `message` rows are the
//! conversation as the model saw it, each record once:
//!
//! | Record | Message |
//! |---|---|
//! | `response_item` `message`, role `user`, typed by a person | user, text |
//! | `response_item` `message`, role `user`, written by the harness | user, meta |
//! | `response_item` `message`, role `developer` | system, meta |
//! | `response_item` `message`, role `assistant` | assistant, text |
//! | `response_item` `reasoning` with a summary | assistant, thinking |
//! | `response_item` `custom_tool_call`, `function_call`, `local_shell_call` | assistant, tool_use |
//! | `response_item` `custom_tool_call_output`, `function_call_output` | user, tool_result |
//! | `compacted` with a message | user, compact_summary |
//!
//! Every other record is an entry and no message. That holds for the
//! `event_msg` records in particular: an `item_completed` event
//! repeats a message, or records what a tool call ran, which is the
//! `tool_use` table's to tell.

use std::path::Path;

use chrono::DateTime;
use gage_corpus::{
    CorpusSessionAttrs, DerivedRows, DerivedSession, Fingerprint, Result, SessionReader,
};
use gage_session::{Entry, Message};
use serde_json::{Map, Value};

use crate::codec::codec;
use crate::tool_use::ToolUseCollector;

/// The generation of the attributes [`derive_session`] produces.
/// Raised on any change to what they hold or mean.
pub(crate) const SUMMARY_VERSION: u32 = 1;

/// Maximum number of characters of a title taken from the first
/// prompt
const MAX_TITLE_CHARS: usize = 60;

/// Parse the rollout at `path` and derive its rows and attributes.
/// `project` is the session's project name, which the listing reads
/// from the rollout's first record.
pub(crate) fn derive_session(id: &str, path: &Path, project: String) -> Result<DerivedSession> {
    let fingerprint = Fingerprint::stat(path)?;
    derive_session_from(
        id,
        SessionReader::open(path, codec())?,
        fingerprint,
        project,
    )
}

/// Derive the rows and attributes of the rollout `reader` yields.
/// `fingerprint` and `project` are the facts the caller holds about
/// the content.
pub(crate) fn derive_session_from<R: std::io::Read>(
    id: &str,
    reader: SessionReader<R>,
    fingerprint: Fingerprint,
    project: String,
) -> Result<DerivedSession> {
    let mut rows = DerivedRows::new();
    let mut tool_uses = ToolUseCollector::new();
    let mut attrs = CorpusSessionAttrs {
        native_mtime_ms: fingerprint.mtime_ms,
        native_size: fingerprint.size,
        project,
        is_empty: true,
        ..Default::default()
    };
    let mut usage: Option<TokenUsage> = None;

    for result in reader {
        // A line that does not read or decode fails the session. The
        // codec stands in for the corruption it recognizes with an
        // invalid entry, which derives a row like any other line.
        let (line_num, raw, decoded) = result?;
        let line = Value::from(decoded);

        attrs.line_count = u64::from(line_num);
        if let Some(ms) = timestamp_ms(&line) {
            attrs.first_timestamp_ms = Some(attrs.first_timestamp_ms.map_or(ms, |f| f.min(ms)));
            attrs.last_timestamp_ms = Some(attrs.last_timestamp_ms.map_or(ms, |l| l.max(ms)));
        }
        if attrs.model.is_none() && line_type(&line) == Some("turn_context") {
            attrs.model = line
                .pointer("/payload/model")
                .and_then(Value::as_str)
                .map(String::from);
        }
        if let Some(total) = thread_token_usage(&line) {
            usage = Some(total);
        }

        if let Some(entry) = derive_entry(id, line_num, &line, raw) {
            if let Some(message) = &entry.message {
                let kind = (message.message_type.as_str(), message.subtype.as_deref());
                if matches!(kind.0, "user" | "assistant") {
                    attrs.message_count += 1;
                }
                if matches!(kind, ("user", Some("text")) | ("assistant", _)) {
                    attrs.is_empty = false;
                }
                if attrs.title.is_none() && kind == ("user", Some("text")) {
                    attrs.title = title_of(&message.text);
                }
            }
            rows.push(id, &entry);
        }
        tool_uses.push(line_num, &line);
    }

    if let Some(usage) = usage {
        // Codex counts cached and cache-written prompt tokens inside
        // its input total; the attributes hold the three apart.
        attrs.input_tokens = (usage.input - usage.cached - usage.cache_write).max(0);
        attrs.cache_read_input_tokens = usage.cached;
        attrs.cache_creation_input_tokens = usage.cache_write;
        attrs.output_tokens = usage.output;
    }
    DerivedSession::new(id, rows, tool_uses.finish(), attrs, fingerprint)
}

/// A title from a prompt: its first non-empty line, cut to
/// [`MAX_TITLE_CHARS`] characters.
fn title_of(prompt: &str) -> Option<String> {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= MAX_TITLE_CHARS {
        return Some(line.to_string());
    }
    let cut: String = line.chars().take(MAX_TITLE_CHARS).collect();
    Some(format!("{}...", cut.trim_end()))
}

struct TokenUsage {
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
}

/// The thread's running token totals as a line records them: the
/// `thread_token_usage` of a `token_usage_record`, or the
/// `total_token_usage` of a `token_count` event. The totals are
/// cumulative, so the last one read is the session's.
fn thread_token_usage(line: &Value) -> Option<TokenUsage> {
    let totals = match (line_type(line)?, payload_type(line)) {
        ("token_usage_record", _) => line.pointer("/payload/thread_token_usage")?,
        ("event_msg", Some("token_count")) => line.pointer("/payload/info/total_token_usage")?,
        _ => return None,
    };
    let count = |key| totals.get(key).and_then(Value::as_i64).unwrap_or(0);
    Some(TokenUsage {
        input: count("input_tokens"),
        cached: count("cached_input_tokens"),
        cache_write: count("cache_write_input_tokens"),
        output: count("output_tokens"),
    })
}

pub(crate) fn line_type(line: &Value) -> Option<&str> {
    line.get("type").and_then(Value::as_str)
}

pub(crate) fn payload_type(line: &Value) -> Option<&str> {
    line.pointer("/payload/type").and_then(Value::as_str)
}

/// The line's `timestamp` as epoch milliseconds; `None` when absent
/// or not RFC 3339.
pub(crate) fn timestamp_ms(line: &Value) -> Option<i64> {
    let s = line.get("timestamp").and_then(Value::as_str)?;
    Some(DateTime::parse_from_rfc3339(s).ok()?.timestamp_millis())
}

/// Normalize one parsed rollout line into `entry` table shape, with
/// its message when the line is one the `message` table holds.
/// `session_id` labels diagnostics only. A line without a `type` is
/// not an entry and yields `None`. `raw` is the line's text as the
/// codec renders it, the `raw` column.
pub(crate) fn derive_entry(
    session_id: &str,
    line_num: u32,
    line: &Value,
    raw: String,
) -> Option<Entry> {
    let Some(entry_type) = line_type(line) else {
        tracing::warn!(session_id, line = line_num, "entry without a type");
        return None;
    };
    Some(Entry {
        line: line_num,
        uuid: (entry_type == "response_item")
            .then(|| line.pointer("/payload/id").and_then(Value::as_str))
            .flatten()
            .map(String::from),
        entry_type: entry_type.to_string(),
        subtype: payload_type(line).map(String::from),
        timestamp_ms: timestamp_ms(line),
        raw: Some(raw),
        message: message_of(line_num, line),
    })
}

/// The message a rollout line is, when it is one.
fn message_of(line_num: u32, line: &Value) -> Option<Message> {
    let payload = line.get("payload")?;
    let message = |message_type: &str, subtype: &str, text: String| Message {
        message_type: message_type.to_string(),
        subtype: Some(subtype.to_string()),
        text,
        attachments: None,
        ide_tags: None,
    };
    match (line_type(line)?, payload_type(line)) {
        ("response_item", Some("message")) => {
            let text = content_text(payload.get("content"));
            let (message_type, subtype) = match payload.get("role").and_then(Value::as_str)? {
                "user" if is_prompt(payload, &text) => ("user", "text"),
                "user" => ("user", "meta"),
                "assistant" => ("assistant", "text"),
                "developer" | "system" => ("system", "meta"),
                _ => return None,
            };
            Some(Message {
                attachments: attachments(line_num, payload.get("content")),
                ..message(message_type, subtype, text)
            })
        }
        ("response_item", Some("reasoning")) => {
            let text = reasoning_text(payload);
            (!text.is_empty()).then(|| message("assistant", "thinking", text))
        }
        ("response_item", Some("custom_tool_call")) => {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
            let input = payload.get("input").cloned().unwrap_or(Value::Null);
            Some(message(
                "assistant",
                "tool_use",
                tool_call_text(name, &input),
            ))
        }
        ("response_item", Some("function_call")) => {
            let input = match payload.get("arguments") {
                // The arguments are JSON held in a string
                Some(Value::String(s)) => {
                    serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
                }
                Some(other) => other.clone(),
                None => Value::Null,
            };
            Some(message(
                "assistant",
                "tool_use",
                tool_call_text(&function_name(payload), &input),
            ))
        }
        ("response_item", Some("local_shell_call")) => {
            let input = payload.get("action").cloned().unwrap_or(Value::Null);
            Some(message(
                "assistant",
                "tool_use",
                tool_call_text("local_shell", &input),
            ))
        }
        ("response_item", Some("custom_tool_call_output" | "function_call_output")) => Some(
            message("user", "tool_result", content_text(payload.get("output"))),
        ),
        ("compacted", _) => {
            let text = payload.get("message").and_then(Value::as_str)?;
            (!text.trim().is_empty()).then(|| message("user", "compact_summary", text.to_string()))
        }
        _ => None,
    }
}

/// The name a function call names its tool by: the tool's name under
/// its namespace, as the model addresses an MCP tool.
pub(crate) fn function_name(payload: &Value) -> String {
    let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
    match payload.get("namespace").and_then(Value::as_str) {
        Some(namespace) if !namespace.is_empty() => format!("{namespace}__{name}"),
        _ => name.to_string(),
    }
}

/// Whether a `user`-role message is what a person typed. Codex writes
/// its own context into the conversation under the same role, and
/// marks each content item with where it came from: a kind under
/// `user.`, or a provenance of `user`. A record without those marks
/// is a prompt unless it opens with a tag, which is how the harness
/// wraps what it writes.
fn is_prompt(payload: &Value, text: &str) -> bool {
    let Some(marks) = payload.get("internal_chat_message_metadata_passthrough") else {
        return !text.trim_start().starts_with('<');
    };
    let kinds = marks
        .get("content_item_kinds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let provenances = marks
        .get("content_item_metadata")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.pointer("/provenance/type").and_then(Value::as_str));
    let mut kinds = kinds;
    let mut provenances = provenances;
    kinds.any(|kind| kind.starts_with("user.")) || provenances.any(|p| p == "user")
}

/// The text of a message's content or a tool call's output: a string
/// as it is, or the `text` of each item of an array, joined by blank
/// lines.
pub(crate) fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// The content items of a message that are not text, each with a
/// `ref` of `[line, content_index]`, as a JSON array; `None` when
/// every item is text.
fn attachments(line_num: u32, content: Option<&Value>) -> Option<String> {
    let items = content?.as_array()?;
    let attached: Vec<Value> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.get("text").is_none())
        .map(|(index, item)| {
            let mut item = item.clone();
            if let Some(object) = item.as_object_mut() {
                object.insert("ref".to_string(), serde_json::json!([line_num, index]));
            }
            item
        })
        .collect();
    (!attached.is_empty()).then(|| Value::Array(attached).to_string())
}

/// The readable text of a reasoning record: its summary, then any
/// reasoning text beside it. The encrypted content is not text.
fn reasoning_text(payload: &Value) -> String {
    ["summary", "content"]
        .iter()
        .filter_map(|key| payload.get(key))
        .map(|items| content_text(Some(items)))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The text of a tool call: a YAML mapping of the tool's name to its
/// input.
pub(crate) fn tool_call_text(name: &str, input: &Value) -> String {
    let mut call = Map::new();
    call.insert(name.to_string(), input.clone());
    match serde_yaml::to_string(&Value::Object(call)) {
        Ok(yaml) => yaml.trim_end().to_string(),
        // A JSON value always has a YAML form; the call's JSON stands
        // in for one that somehow does not.
        Err(_) => format!("{name}: {input}"),
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{Array, StringArray};
    use gage_corpus::{COL_MESSAGE_SUBTYPE, COL_MESSAGE_TYPE, COL_SUBTYPE, COL_TEXT, COL_TYPE};
    use gage_session::SessionAttrs;
    use serde_json::json;

    use super::*;
    use crate::testing::{ROLLOUT, THREAD_ID, lines};

    fn derived() -> DerivedSession {
        let fingerprint = Fingerprint {
            mtime_ms: 1,
            size: ROLLOUT.len() as u64,
        };
        derive_session_from(
            THREAD_ID,
            SessionReader::new(ROLLOUT.as_bytes(), codec()),
            fingerprint,
            "-home-u-proj".to_string(),
        )
        .unwrap()
    }

    /// The message of each line that is one, as `(line, type, subtype)`
    fn messages() -> Vec<(u32, String, String)> {
        lines()
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let line_num = i as u32 + 1;
                let message = derive_entry("s", line_num, line, String::new())?.message?;
                Some((line_num, message.message_type, message.subtype?))
            })
            .collect()
    }

    #[test]
    fn each_record_of_the_conversation_is_one_message() {
        let expected = [
            (4, "system", "meta"),
            (5, "user", "meta"),
            (7, "user", "text"),
            (9, "assistant", "thinking"),
            (11, "assistant", "text"),
            (12, "assistant", "tool_use"),
            (15, "user", "tool_result"),
            (16, "assistant", "tool_use"),
            (19, "user", "tool_result"),
            (20, "assistant", "text"),
        ]
        .map(|(line, type_, subtype)| (line, type_.to_string(), subtype.to_string()));
        assert_eq!(
            messages(),
            expected,
            "an item_completed event repeats a message or records a tool call, and is no message"
        );
    }

    #[test]
    fn an_entry_keeps_the_types_codex_names() {
        let derived = derived();
        let column = |i: usize| {
            derived
                .batch
                .column(i)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .clone()
        };
        assert_eq!(derived.batch.num_rows(), 23);
        // Line 7, the prompt, and line 8, the event that repeats it
        assert_eq!(column(COL_TYPE).value(6), "response_item");
        assert_eq!(column(COL_SUBTYPE).value(6), "message");
        assert_eq!(column(COL_MESSAGE_TYPE).value(6), "user");
        assert_eq!(column(COL_MESSAGE_SUBTYPE).value(6), "text");
        assert_eq!(column(COL_TEXT).value(6), "Fix the greeting\nplease");
        assert_eq!(column(COL_TYPE).value(7), "event_msg");
        assert_eq!(column(COL_SUBTYPE).value(7), "item_completed");
        assert!(column(COL_MESSAGE_TYPE).is_null(7));
        assert!(column(COL_TEXT).is_null(7));
    }

    #[test]
    fn message_text_is_what_a_reader_searches() {
        let text = |line: usize| {
            let entry = derive_entry("s", line as u32, &lines()[line - 1], String::new());
            entry.unwrap().message.unwrap().text
        };
        assert_eq!(
            text(9),
            "Plan the edit",
            "the summary, not the encrypted content"
        );
        assert!(text(12).starts_with("exec: |"), "{}", text(12));
        assert!(text(12).contains("tools.exec_command"));
        assert_eq!(
            text(15),
            "Script completed\nWall time 0.1 seconds\nOutput:\n\n\n0"
        );
    }

    #[test]
    fn a_user_message_without_marks_is_a_prompt_unless_it_opens_with_a_tag() {
        let message = |text: &str| {
            let line = json!({
                "timestamp": "2026-01-02T03:04:05.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}],
                },
            });
            derive_entry("s", 1, &line, String::new())
                .unwrap()
                .message
                .unwrap()
                .subtype
        };
        assert_eq!(message("hello").as_deref(), Some("text"));
        assert_eq!(message("<environment_context>x").as_deref(), Some("meta"));
    }

    #[test]
    fn reasoning_with_no_summary_is_an_entry_and_no_message() {
        let line = json!({
            "type": "response_item",
            "payload": {"type": "reasoning", "summary": [], "encrypted_content": "x"},
        });
        let entry = derive_entry("s", 1, &line, String::new()).unwrap();
        assert_eq!(entry.subtype.as_deref(), Some("reasoning"));
        assert_eq!(entry.message, None);
    }

    #[test]
    fn attributes_summarize_the_session() {
        let attrs = derived().attrs;
        assert_eq!(attrs.title(), Some("Fix the greeting"));
        assert_eq!(attrs.model(), Some("gpt-test"));
        assert_eq!(attrs.project_name(), Some("-home-u-proj"));
        assert_eq!(attrs.line_count(), Some(23));
        assert_eq!(
            attrs.message_count(),
            Some(9),
            "the user and assistant messages; the developer message is the harness's"
        );
        assert_eq!(attrs.is_empty(), Some(false));
        assert_eq!(attrs.cost_usd(), None);
        // The last running total: 300 prompt tokens, 120 of them read
        // from the cache
        assert_eq!(attrs.input_tokens(), Some(300));
        assert_eq!(attrs.cached_tokens(), Some(120));
        assert_eq!(attrs.output_tokens(), Some(25));
        assert_eq!(attrs.first_timestamp_ms, Some(1_767_323_041_000));
        assert_eq!(attrs.last_timestamp_ms, Some(1_767_323_063_000));
    }

    #[test]
    fn a_session_with_only_harness_context_is_empty() {
        let text: String = ROLLOUT.lines().take(6).map(|l| format!("{l}\n")).collect();
        let fingerprint = Fingerprint {
            mtime_ms: 1,
            size: text.len() as u64,
        };
        let derived = derive_session_from(
            THREAD_ID,
            SessionReader::new(text.as_bytes(), codec()),
            fingerprint,
            String::new(),
        )
        .unwrap();
        assert_eq!(derived.attrs.is_empty(), Some(true));
        assert_eq!(derived.attrs.title(), None);
        assert_eq!(derived.attrs.message_count(), Some(1));
    }
}
