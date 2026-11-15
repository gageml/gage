//! The body a viewer shows for one record of a Codex rollout.
//!
//! A body is for reading through a session, not for inspecting a
//! record: it shows what tells a reader what happened and leaves the
//! rest to the record's source.

use gage_session::{BodyAttr, BodyBlock, EntryBody, EntryValue, INVALID_ENTRY_TYPE};
use serde_json::Value;

use crate::derive::{content_text, function_name};

pub(crate) fn entry_body(entry: &EntryValue) -> EntryBody {
    let line = entry.to_json_value();
    let payload = line.get("payload").unwrap_or(&Value::Null);
    let blocks = match line.get("type").and_then(Value::as_str) {
        Some(INVALID_ENTRY_TYPE) => invalid_entry(&line),
        Some("response_item") => response_item(payload),
        Some("event_msg") => event(payload),
        Some("session_meta") => rows(attrs(
            payload,
            &[
                "id",
                "cwd",
                "cli_version",
                "originator",
                "source",
                "thread_source",
                "model_provider",
                "history_mode",
            ],
        )),
        Some("turn_context") => rows(attrs(payload, &["model", "effort", "cwd"])),
        Some("token_usage_record") => rows(token_attrs(payload.get("thread_token_usage"))),
        Some("compacted") => text_or_empty(str_at(payload, "message").unwrap_or_default()),
        Some(_) => rows(scalar_members(payload)),
        None => Vec::new(),
    };
    EntryBody { blocks }
}

/// The codec's error, then the line it could not decode.
fn invalid_entry(line: &Value) -> Vec<BodyBlock> {
    let field = |key| str_at(line, key).unwrap_or_default().to_string();
    vec![
        BodyBlock::Text(field("error")),
        BodyBlock::Text(field("raw")),
    ]
}

/// A record of the conversation: a message's text, a reasoning
/// summary, a tool call's input, or a tool call's output.
fn response_item(payload: &Value) -> Vec<BodyBlock> {
    match str_at(payload, "type") {
        Some("message") => text_or_empty(&content_text(payload.get("content"))),
        Some("reasoning") => {
            let summary = content_text(payload.get("summary"));
            match summary.trim().is_empty() {
                true => vec![BodyBlock::Remark("Reasoning not recorded as text".into())],
                false => vec![BodyBlock::Markdown(summary)],
            }
        }
        Some("custom_tool_call") => tool_call(
            str_at(payload, "name").unwrap_or_default(),
            str_at(payload, "input").unwrap_or_default().to_string(),
        ),
        Some("function_call") => tool_call(
            &function_name(payload),
            str_at(payload, "arguments").unwrap_or_default().to_string(),
        ),
        Some("local_shell_call") => tool_call(
            "local_shell",
            payload.get("action").map(pretty).unwrap_or_default(),
        ),
        Some("custom_tool_call_output" | "function_call_output") => {
            let output = content_text(payload.get("output"));
            match output.trim().is_empty() {
                true => vec![BodyBlock::Remark("Empty".into())],
                false => vec![BodyBlock::Text(output)],
            }
        }
        Some("additional_tools") => {
            let count = payload
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            vec![BodyBlock::Remark(format!("{count} tool definitions"))]
        }
        _ => rows(scalar_members(payload)),
    }
}

fn tool_call(name: &str, input: String) -> Vec<BodyBlock> {
    let mut blocks = vec![BodyBlock::Attrs(vec![BodyAttr::new("tool", name)])];
    if !input.trim().is_empty() {
        blocks.push(BodyBlock::Text(input));
    }
    blocks
}

/// An event: what an item records, the token totals, or the event's
/// own scalar fields.
fn event(payload: &Value) -> Vec<BodyBlock> {
    match str_at(payload, "type") {
        Some("item_completed") => item(payload.get("item").unwrap_or(&Value::Null)),
        Some("token_count") => rows(token_attrs(payload.pointer("/info/total_token_usage"))),
        _ => rows(scalar_members(payload)),
    }
}

/// An item: the message it repeats, or what the tool call it records
/// ran and returned.
fn item(item: &Value) -> Vec<BodyBlock> {
    match str_at(item, "type") {
        Some("UserMessage" | "AgentMessage") => text_or_empty(&content_text(item.get("content"))),
        Some("Reasoning") => {
            let summary: Vec<&str> = item
                .get("summary_text")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            text_or_empty(&summary.join("\n\n"))
        }
        Some("CommandExecution") => {
            let command = item
                .get("command")
                .and_then(Value::as_array)
                .and_then(|words| words.last())
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut top = vec![BodyAttr::new("command", command)];
            top.extend(attrs(item, &["cwd", "exit_code"]));
            with_text(top, str_at(item, "aggregated_output"))
        }
        Some("McpToolCall") => {
            let mut top = attrs(item, &["server", "tool", "status"]);
            if let Some(arguments) = item.get("arguments").filter(|a| !a.is_null()) {
                top.push(BodyAttr::new("arguments", arguments.to_string()));
            }
            let result = match item.pointer("/error/message").and_then(Value::as_str) {
                Some(error) => error.to_string(),
                None => content_text(item.pointer("/result/content")),
            };
            with_text(top, Some(&result))
        }
        Some("FileChange") => file_change(item),
        _ => rows(scalar_members(item)),
    }
}

/// A file change: each path under a heading, with the diff of a file
/// updated or the content of one added or deleted.
fn file_change(item: &Value) -> Vec<BodyBlock> {
    let mut blocks = rows(attrs(item, &["status"]));
    let changes = item.get("changes").and_then(Value::as_object);
    for (path, change) in changes.into_iter().flatten() {
        let kind = str_at(change, "type").unwrap_or("change");
        blocks.push(BodyBlock::Heading(format!("{kind} {path}")));
        let text = str_at(change, "unified_diff").or_else(|| str_at(change, "content"));
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            blocks.push(BodyBlock::Text(text.to_string()));
        }
    }
    blocks
}

fn text_or_empty(text: &str) -> Vec<BodyBlock> {
    match text.trim().is_empty() {
        true => vec![BodyBlock::Remark("Empty".into())],
        false => vec![BodyBlock::Markdown(text.to_string())],
    }
}

fn with_text(top: Vec<BodyAttr>, text: Option<&str>) -> Vec<BodyBlock> {
    let mut blocks = rows(top);
    if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
        blocks.push(BodyBlock::Text(text.to_string()));
    }
    blocks
}

fn rows(attrs: Vec<BodyAttr>) -> Vec<BodyBlock> {
    match attrs.is_empty() {
        true => Vec::new(),
        false => vec![BodyBlock::Attrs(attrs)],
    }
}

/// The token totals of a usage record as rows.
fn token_attrs(usage: Option<&Value>) -> Vec<BodyAttr> {
    let Some(usage) = usage else {
        return Vec::new();
    };
    attrs(
        usage,
        &[
            "input_tokens",
            "cached_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
            "total_tokens",
        ],
    )
}

/// One row per named member of `value` that is a scalar.
fn attrs(value: &Value, keys: &[&str]) -> Vec<BodyAttr> {
    keys.iter()
        .filter_map(|key| Some(BodyAttr::new(*key, scalar(value.get(*key)?)?)))
        .collect()
}

/// One row per member of `value` that is a scalar, in the record's
/// order of keys, without the record's own `type`.
fn scalar_members(value: &Value) -> Vec<BodyAttr> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.as_str() != "type")
        .filter_map(|(key, member)| Some(BodyAttr::new(key.as_str(), scalar(member)?)))
        .collect()
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
