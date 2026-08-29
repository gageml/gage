//! Helpers for rendering Claude Code session entries as readable text.

use serde_json::Value;

/// Render a single message-content block as human-readable text.
///
/// Each block has a `type` field; this function dispatches on it and
/// returns a sensible text rendering for the type. Unknown types fall
/// back to `<type-name>`. Empty or missing fields render as empty
/// string.
pub fn block_to_text(block: &Value) -> String {
    let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "text" => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "thinking" => block
            .get("thinking")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "tool_use" => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
            let input = block.get("input").map(Value::to_string).unwrap_or_default();
            format!("<tool_use: {name}> {input}")
        }
        "tool_result" => match block.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr.iter().map(block_to_text).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        },
        "image" => "<image>".into(),
        "redacted_thinking" => "<redacted thinking>".into(),
        "" => String::new(),
        other => format!("<{other}>"),
    }
}

/// Render the message content of a session entry as readable text.
///
/// Joins every block in `message.content` via `block_to_text`, separated
/// by newlines. Entries without a `message.content` array return an
/// empty string. Entries whose `message.content` is a plain string
/// return that string.
pub fn entry_to_text(entry: &Value) -> String {
    match entry.get("message").and_then(|m| m.get("content")) {
        Some(Value::Array(arr)) => arr.iter().map(block_to_text).collect::<Vec<_>>().join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Return the raw `subtype` JSON property of a session entry, verbatim.
///
/// This is the literal value present in the JSONL line, not a derived
/// classification. Most entry types do not carry a `subtype` and this
/// returns `None` for them. Compare with [`message_subtype`], which
/// computes a content-shape classification for `user`/`assistant`
/// entries.
pub fn entry_subtype(entry: &Value) -> Option<&str> {
    entry.get("subtype").and_then(Value::as_str)
}

/// System entry subtypes promoted to message rows. Each carries
/// content that is a meaningful injection into the user/assistant
/// message flow, and each has hard-coded text rendering in the
/// derivation (gage-index). Unknown system subtypes stay entry-only;
/// bookkeeping subtypes (`turn_duration`, `stop_hook_summary`) are
/// deliberately excluded.
pub const SYSTEM_MESSAGE_SUBTYPES: &[&str] = &[
    "api_error",
    "away_summary",
    "compact_boundary",
    "informational",
    "local_command",
];

/// Infer the subtype of a message-shaped session entry.
///
/// - `assistant` → `tool_use` if any content block is `tool_use`,
///   else `thinking` if any is `thinking`, else `text`.
/// - `user` → `tool_result` if any content block is `tool_result`,
///   else `meta` if `entry.isMeta` is true, else `text`.
/// - `system` → the raw subtype when it is one of
///   [`SYSTEM_MESSAGE_SUBTYPES`].
/// - `attachment` → the inner `attachment.type`
///   (e.g. `deferred_tools_delta`, `skill_listing`).
/// - Other entry types have no message subtype.
pub fn message_subtype(entry: &Value) -> Option<&'static str> {
    let ty = entry.get("type").and_then(Value::as_str)?;
    let blocks: Vec<&str> = entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("type").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    match ty {
        "assistant" => {
            if blocks.contains(&"tool_use") {
                Some("tool_use")
            } else if blocks.contains(&"thinking") {
                Some("thinking")
            } else {
                Some("text")
            }
        }
        "user" => {
            if blocks.contains(&"tool_result") {
                Some("tool_result")
            } else if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
                Some("meta")
            } else {
                Some("text")
            }
        }
        "system" => {
            let sub = entry_subtype(entry)?;
            SYSTEM_MESSAGE_SUBTYPES.iter().find(|s| **s == sub).copied()
        }
        "attachment" => {
            let inner = entry
                .get("attachment")
                .and_then(|a| a.get("type"))
                .and_then(Value::as_str)?;
            // Enumerated so the return stays `&'static str` (no
            // allocation). Add known inner types as we encounter them.
            match inner {
                "deferred_tools_delta" => Some("deferred_tools_delta"),
                "skill_listing" => Some("skill_listing"),
                "mcp_instructions_delta" => Some("mcp_instructions_delta"),
                "max_turns_reached" => Some("max_turns_reached"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Return refs to every content block whose `type` is not `text`. These
/// are the blocks gage-query persists as `attachments`. Returns empty
/// when `message.content` is absent, a string, or all-text.
pub fn entry_attachment_blocks(entry: &Value) -> Vec<&Value> {
    entry
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) != Some("text"))
                .filter(|b| b.get("type").is_some())
                .collect()
        })
        .unwrap_or_default()
}

/// Tag names [`split_ide_tags`] recognizes as out-of-band data
/// prepended by the client, harness, or slash-command machinery.
/// Restricting the split to known names keeps tag-shaped content —
/// a `<tool_use_error>` tool result, pasted XML — in the body.
const IDE_TAG_NAMES: &[&str] = &[
    "system-reminder",
    "ide_opened_file",
    "ide_selection",
    "command-name",
    "command-message",
    "command-args",
    "command-contents",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "task-notification",
];

/// Split a message text into `(body, ide_tags)`. `ide_tags` captures
/// any leading `<tag>...</tag>` pairs prepended by the client or IDE
/// (e.g. `<ide_opened_file>`, `<system-reminder>`,
/// `<local-command-caveat>`); the body is the remaining text with
/// leading whitespace consumed. Only tag names in [`IDE_TAG_NAMES`]
/// are split. Returns `(input.to_string(), None)` when no leading
/// recognized tag pair is present.
///
/// "IDE" understates the source — these tags also come from the
/// harness and slash-command machinery — but in every case they are
/// out-of-band data prepended to the user's actual prompt.
pub fn split_ide_tags(input: &str) -> (String, Option<String>) {
    let start = input
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(input.len());
    let mut cursor = start;

    loop {
        let rest = &input[cursor..];
        let after_lt = match rest.strip_prefix('<') {
            Some(r) => r,
            None => break,
        };
        let name_end = after_lt
            .find(|c: char| c == '>' || c == '/' || c.is_whitespace())
            .unwrap_or(after_lt.len());
        let tag_name = &after_lt[..name_end];
        if !IDE_TAG_NAMES.contains(&tag_name) {
            break;
        }
        let close = format!("</{}>", tag_name);
        let close_rel = match rest.find(&close) {
            Some(i) => i,
            None => break,
        };
        cursor += close_rel + close.len();
        let after_close = &input[cursor..];
        let ws = after_close
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(after_close.len());
        cursor += ws;
    }

    if cursor == start {
        return (input.to_string(), None);
    }

    let tags = input[..cursor].trim().to_string();
    let body = input[cursor..].to_string();
    let ide_tags = if tags.is_empty() { None } else { Some(tags) };
    (body, ide_tags)
}

/// Resolve a dotted `field_path` to a JSON value within an entry.
///
/// Example: `"message.content"` → `entry["message"]["content"]`.
/// Returns None if any segment is missing or traverses a non-object.
pub fn resolve_field<'a>(entry: &'a Value, field_path: &str) -> Option<&'a Value> {
    let mut cur = entry;
    for segment in field_path.split('.') {
        cur = cur.get(segment)?;
    }
    Some(cur)
}
