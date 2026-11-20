//! The body a viewer shows for one entry of a Pi session.
//!
//! A body is for reading through a session, not for inspecting an
//! entry: it shows what tells a reader what happened and leaves the
//! rest to the entry's source.

use gage_session::{BodyAttr, BodyBlock, EntryBody, EntryValue, INVALID_ENTRY_TYPE};
use serde_json::Value;

use crate::derive::{blocks, content_text, str_at};

pub(crate) fn entry_body(entry: &EntryValue) -> EntryBody {
    let line = entry.to_json_value();
    let blocks = match str_at(&line, "type") {
        Some(INVALID_ENTRY_TYPE) => invalid_entry(&line),
        Some("session") => rows(attrs(
            &line,
            &["id", "version", "cwd", "parentSession", "branchedFrom"],
        )),
        Some("message") => message(line.get("message").unwrap_or(&Value::Null)),
        Some("usage") => {
            let mut top = attrs(&line, &["kind", "provider", "model", "note"]);
            top.extend(usage_attrs(line.get("usage")));
            // The cost as Pi wrote it: a float that small prints in
            // another form than the file holds.
            if let Some(EntryValue::Number(cost)) = entry.pointer("/usage/cost/total") {
                top.push(BodyAttr::new("cost", cost.as_str()));
            }
            rows(top)
        }
        Some("compaction") => with_markdown(
            attrs(&line, &["tokensBefore"]),
            str_at(&line, "summary").unwrap_or_default(),
        ),
        Some("branch_summary") => {
            with_markdown(Vec::new(), str_at(&line, "summary").unwrap_or_default())
        }
        Some("custom") => {
            let mut blocks = rows(attrs(&line, &["customType"]));
            if let Some(data) = line.get("data").filter(|data| !data.is_null()) {
                blocks.push(BodyBlock::Text(pretty(data)));
            }
            blocks
        }
        Some("custom_message") => with_markdown(
            attrs(&line, &["customType"]),
            &content_text(line.get("content")),
        ),
        Some("context_edit") => context_edit(&line),
        Some(_) => rows(scalar_members(&line)),
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

/// A message, by its role.
fn message(message: &Value) -> Vec<BodyBlock> {
    let content = message.get("content");
    match str_at(message, "role") {
        Some("assistant") => assistant(message),
        Some("toolResult") => {
            let mut top = attrs(message, &["toolName"]);
            if message.get("isError").and_then(Value::as_bool) == Some(true) {
                top.push(BodyAttr::new("error", "true"));
            }
            let mut blocks = rows(top);
            let output = content_text(content);
            blocks.push(match output.trim().is_empty() {
                true => BodyBlock::Remark("Empty".into()),
                false => BodyBlock::Text(output),
            });
            blocks.extend(image_remark(content));
            blocks
        }
        Some("bashExecution") => {
            let mut blocks = rows(attrs(message, &["command", "exitCode"]));
            if let Some(output) = str_at(message, "output").filter(|o| !o.trim().is_empty()) {
                blocks.push(BodyBlock::Text(output.to_string()));
            }
            blocks
        }
        Some("system") => system(message),
        Some(_) => {
            let mut blocks = text_or_empty(&content_text(content));
            blocks.extend(image_remark(content));
            blocks
        }
        None => Vec::new(),
    }
}

/// An assistant response: each block in order, then the error that
/// ended it when one did.
fn assistant(message: &Value) -> Vec<BodyBlock> {
    let mut out = Vec::new();
    for block in blocks(message.get("content")) {
        match str_at(block, "type") {
            Some("text") => out.extend(
                str_at(block, "text")
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| BodyBlock::Markdown(text.to_string())),
            ),
            Some("thinking") => out.push(
                match str_at(block, "thinking").filter(|t| !t.trim().is_empty()) {
                    Some(thinking) => BodyBlock::Markdown(thinking.to_string()),
                    None => BodyBlock::Remark("Thinking not recorded as text".into()),
                },
            ),
            Some("toolCall") => {
                let name = str_at(block, "name").unwrap_or_default();
                out.push(BodyBlock::Attrs(vec![BodyAttr::new("tool", name)]));
                if let Some(arguments) = block.get("arguments").filter(|a| !a.is_null()) {
                    out.push(BodyBlock::Text(pretty(arguments)));
                }
            }
            _ => {}
        }
    }
    if let Some(error) = str_at(message, "errorMessage").filter(|e| !e.is_empty()) {
        out.push(BodyBlock::Text(error.to_string()));
    }
    if out.is_empty() {
        out.push(BodyBlock::Remark("Empty".into()));
    }
    out
}

/// A system message: its content, each prompt section it sets or
/// removes under the section's name, and how many tools it adds and
/// removes.
fn system(message: &Value) -> Vec<BodyBlock> {
    let mut out = Vec::new();
    let content = content_text(message.get("content"));
    if !content.trim().is_empty() {
        out.push(BodyBlock::Markdown(content));
    }
    let sections = message.get("sections").and_then(Value::as_object);
    for (name, section) in sections.into_iter().flatten() {
        out.push(BodyBlock::Heading(name.clone()));
        out.push(match section.as_str() {
            Some(text) => BodyBlock::Text(text.to_string()),
            None => BodyBlock::Remark("Removed".into()),
        });
    }
    for (key, what) in [("toolsAdded", "added"), ("toolsRemoved", "removed")] {
        let count = message
            .get(key)
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if count > 0 {
            out.push(BodyBlock::Remark(format!("{count} tools {what}")));
        }
    }
    out
}

/// An edit of what a later request sees of an earlier entry: the
/// replacement, or that the entry is left out.
fn context_edit(line: &Value) -> Vec<BodyBlock> {
    let mut out = rows(attrs(line, &["targetId"]));
    out.push(match line.pointer("/replacement/content") {
        Some(content) => BodyBlock::Text(content_text(Some(content))),
        None => BodyBlock::Remark("Left out of the model's context".into()),
    });
    out
}

/// A remark counting the images among `content`'s blocks, when it
/// holds any.
fn image_remark(content: Option<&Value>) -> Option<BodyBlock> {
    let count = blocks(content)
        .iter()
        .filter(|block| str_at(block, "type") == Some("image"))
        .count();
    (count > 0).then(|| BodyBlock::Remark(format!("{count} images")))
}

fn text_or_empty(text: &str) -> Vec<BodyBlock> {
    match text.trim().is_empty() {
        true => vec![BodyBlock::Remark("Empty".into())],
        false => vec![BodyBlock::Markdown(text.to_string())],
    }
}

fn with_markdown(top: Vec<BodyAttr>, text: &str) -> Vec<BodyBlock> {
    let mut blocks = rows(top);
    blocks.extend(text_or_empty(text));
    blocks
}

fn rows(attrs: Vec<BodyAttr>) -> Vec<BodyBlock> {
    match attrs.is_empty() {
        true => Vec::new(),
        false => vec![BodyBlock::Attrs(attrs)],
    }
}

/// The token counts of a usage record as rows.
fn usage_attrs(usage: Option<&Value>) -> Vec<BodyAttr> {
    let Some(usage) = usage else {
        return Vec::new();
    };
    attrs(
        usage,
        &["input", "output", "cacheRead", "cacheWrite", "totalTokens"],
    )
}

/// One row per named member of `value` that is a scalar.
fn attrs(value: &Value, keys: &[&str]) -> Vec<BodyAttr> {
    keys.iter()
        .filter_map(|key| Some(BodyAttr::new(*key, scalar(value.get(*key)?)?)))
        .collect()
}

/// One row per member of `value` that is a scalar, in the entry's
/// order of keys, without the entry's `type` and its place in the
/// tree.
fn scalar_members(value: &Value) -> Vec<BodyAttr> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !matches!(key.as_str(), "type" | "id" | "parentId" | "timestamp"))
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
