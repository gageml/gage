//! The body a viewer shows for one entry of a Claude Code session.
//!
//! A body is for reading through a session, not for inspecting an
//! entry: it shows what tells a reader what happened and leaves the
//! rest to the entry's source.

use gage_core::human::format_size;
use gage_session::{BodyAttr, BodyBlock, EntryBody, EntryValue, INVALID_ENTRY_TYPE};

use crate::entry::{message_subtype, split_ide_tags};
use crate::index::entry_text;

pub fn entry_body(entry: &EntryValue) -> EntryBody {
    let blocks = match str_at(entry, "/type") {
        Some(INVALID_ENTRY_TYPE) => invalid_entry(entry),
        Some(kind @ ("user" | "assistant")) => message(entry, kind),
        Some("system") => system(entry),
        Some("attachment") => attachment(entry),
        Some("cost-state") => cost_state(entry),
        Some("file-history-snapshot") => file_history_snapshot(entry),
        Some("api-request-blob") => api_request_blob(entry),
        Some("artifact-comment-monitor") => artifact_comment_monitor(entry),
        Some("artifact-autoreact-ledger") => artifact_ids(entry),
        Some(kind) => {
            let (attr_pointers, text_pointers) = selection(kind);
            rows_then_text(attrs(entry, attr_pointers), texts(entry, text_pointers))
        }
        None => Vec::new(),
    };
    EntryBody { blocks }
}

/// The codec's error, then the line it could not decode.
fn invalid_entry(entry: &EntryValue) -> Vec<BodyBlock> {
    let field = |pointer| str_at(entry, pointer).unwrap_or_default().to_string();
    vec![
        BodyBlock::Attrs(vec![BodyAttr::new("Error", field("/error"))]),
        BodyBlock::Text(field("/raw")),
    ]
}

/// A user or assistant message: the text the `message` table holds
/// for it. A message without text shows the tags split off the front
/// of it, else its images, else that it is empty.
fn message(entry: &EntryValue, kind: &str) -> Vec<BodyBlock> {
    let json = entry.to_json_value();
    let subtype = message_subtype(&json);
    let joined = entry_text(&json).unwrap_or_default();
    let (text, tags) = match (kind, subtype) {
        ("user", Some("text" | "meta")) => split_ide_tags(&joined),
        _ => (joined, None),
    };
    if !text.trim().is_empty() {
        return vec![match subtype {
            Some("tool_use" | "tool_result") => BodyBlock::Text(text),
            _ => BodyBlock::Markdown(text),
        }];
    }
    let tags = tags.as_deref().map(tag_pairs).unwrap_or_default();
    if !tags.is_empty() {
        return vec![BodyBlock::Attrs(tags)];
    }
    let images = image_remarks(entry);
    if !images.is_empty() {
        return images;
    }
    vec![BodyBlock::Remark("Empty".to_string())]
}

/// The name and trimmed content of each top-level `<name>…</name>`
/// pair in `tags`, in order. A tag nested in a pair's content stays in
/// the content. Text that does not open a closed pair ends the walk
/// and is kept whole under an empty name, so nothing is left out.
fn tag_pairs(tags: &str) -> Vec<BodyAttr> {
    let mut pairs = Vec::new();
    let mut rest = tags.trim();
    while !rest.is_empty() {
        let Some((name, content, after)) = leading_pair(rest) else {
            pairs.push(BodyAttr::new("", rest));
            break;
        };
        pairs.push(BodyAttr::new(name, content.trim()));
        rest = after.trim_start();
    }
    pairs
}

/// The name and content of the pair `text` opens with, and the text
/// after its close tag.
fn leading_pair(text: &str) -> Option<(&str, &str, &str)> {
    let after_lt = text.strip_prefix('<')?;
    let name_end = after_lt.find(|c: char| c == '>' || c == '/' || c.is_whitespace())?;
    let name = &after_lt[..name_end];
    let (_, after_open) = after_lt.split_once('>')?;
    let (content, after) = after_open.split_once(&format!("</{name}>"))?;
    (!name.is_empty()).then_some((name, content, after))
}

/// One remark per image block of the message, with the size of the
/// image its base64 text decodes to.
fn image_remarks(entry: &EntryValue) -> Vec<BodyBlock> {
    let blocks = entry
        .pointer("/message/content")
        .and_then(EntryValue::as_array);
    blocks
        .into_iter()
        .flatten()
        .filter(|block| str_at(block, "/type") == Some("image"))
        .map(|block| {
            let encoded = str_at(block, "/source/data").map_or(0, str::len);
            let bytes = i64::try_from(encoded / 4 * 3).unwrap_or(i64::MAX);
            BodyBlock::Remark(format!("Image ({})", format_size(bytes)))
        })
        .collect()
}

/// A system entry: the text of a subtype the `message` table holds a
/// row for, else what the subtype's bookkeeping says.
fn system(entry: &EntryValue) -> Vec<BodyBlock> {
    let json = entry.to_json_value();
    if message_subtype(&json).is_some()
        && let Some(text) = entry_text(&json).filter(|t| !t.trim().is_empty())
    {
        return vec![BodyBlock::Markdown(text)];
    }
    let (attr_pointers, text_pointers): (&[&str], &[&str]) = match str_at(entry, "/subtype") {
        Some("turn_duration") => (&["/durationMs", "/messageCount"], &[]),
        Some("stop_hook_summary") => (
            &[
                "/hookCount",
                "/preventedContinuation",
                "/stopReason",
                "/hasOutput",
            ],
            &["/hookErrors"],
        ),
        Some("bridge_status") => (&["/url"], &["/content"]),
        _ => (&[], &["/content"]),
    };
    rows_then_text(attrs(entry, attr_pointers), texts(entry, text_pointers))
}

/// An attachment: its type and subtype, then the text the harness
/// sent to the model. A row without that text, one the harness never
/// sent or one written before the harness recorded `rendered`, shows
/// what its type keeps instead.
fn attachment(entry: &EntryValue) -> Vec<BodyBlock> {
    let kind = str_at(entry, "/attachment/type").unwrap_or_default();
    let mut rows = vec![BodyAttr::new("Type", kind)];
    if let Some(subtype) = attachment_subtype(entry) {
        rows.push(BodyAttr::new("Subtype", subtype));
    }
    let rendered = rendered_texts(entry);
    if !rendered.is_empty() {
        return rows_then_text(rows, rendered);
    }
    let text = match kind {
        "deferred_tools_record" => deferred_tool_names(entry),
        "thinking_drop" => {
            rows.extend(thinking_drop_rows(entry));
            Vec::new()
        }
        "batching_reminder_sent" => texts(entry, &["/attachment/text"]),
        _ => {
            let text_pointers = attachment_text_pointers(kind);
            rows.extend(attachment_scalars(entry, text_pointers));
            texts(entry, text_pointers)
        }
    };
    rows_then_text(rows, text)
}

/// `reminderType` where present, else the hook event and name.
fn attachment_subtype(entry: &EntryValue) -> Option<String> {
    let field = |pointer| str_at(entry, pointer).map(str::to_string);
    field("/attachment/reminderType").or_else(|| {
        match (
            field("/attachment/hookEvent"),
            field("/attachment/hookName"),
        ) {
            (Some(event), Some(name)) => Some(format!("{event} {name}")),
            (event, name) => event.or(name),
        }
    })
}

/// The text of each `rendered` item in order. An item's `content` is a
/// string, or on a queued command a list of text blocks, which are
/// joined. One outer `<system-reminder>` wrapper is removed; the
/// harness puts it around every item, and a reader of the body knows
/// the text is injected.
fn rendered_texts(entry: &EntryValue) -> Vec<String> {
    let items = entry.pointer("/rendered").and_then(EntryValue::as_array);
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let text = match item.get("content")? {
                EntryValue::String(s) => s.clone(),
                EntryValue::Array(blocks) => blocks
                    .iter()
                    .filter_map(|b| str_at(b, "/text"))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                _ => return None,
            };
            let trimmed = text.trim();
            let inner = trimmed
                .strip_prefix("<system-reminder>")
                .and_then(|t| t.strip_suffix("</system-reminder>"))
                .unwrap_or(trimmed);
            Some(inner.trim().to_string())
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// The names of the tool definitions the record holds, one per line.
fn deferred_tool_names(entry: &EntryValue) -> Vec<String> {
    let entries = entry
        .pointer("/attachment/entries")
        .and_then(EntryValue::as_array);
    let names: Vec<&str> = entries
        .into_iter()
        .flatten()
        .filter_map(|tool| str_at(tool, "/name"))
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    vec![names.join("\n")]
}

/// How many thinking blocks the harness left out of a request, over
/// how many turns, and why.
fn thinking_drop_rows(entry: &EntryValue) -> Vec<BodyAttr> {
    let count = |pointer| entry.pointer(pointer).and_then(EntryValue::as_u64);
    let mut rows = Vec::new();
    if let (Some(blocks), Some(turns)) = (
        count("/attachment/newlyDropped/blockCount"),
        count("/attachment/newlyDropped/turnCount"),
    ) {
        rows.push(BodyAttr::new(
            "Dropped",
            format!("{} in {}", counted(blocks, "block"), counted(turns, "turn")),
        ));
    }
    if let Some(reason) = str_at(entry, "/attachment/newlyDropped/reason") {
        rows.push(BodyAttr::new("Reason", reason));
    }
    rows
}

fn counted(count: u64, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// Where an attachment type keeps text a reader wants as text.
fn attachment_text_pointers(kind: &str) -> &'static [&'static str] {
    match kind {
        "total_tokens_reminder" | "silent_turn_reminder" => &["/attachment/text"],
        "prompt_snapshot" => &["/attachment/systemPrompt"],
        "edited_text_file" => &["/attachment/snippet"],
        "nested_memory" => &["/attachment/content/content"],
        "directory" | "skill_listing" | "hook_success" | "hook_additional_context" => {
            &["/attachment/content"]
        }
        "agent_listing_delta" | "deferred_tools_delta" => &["/attachment/addedLines"],
        "mcp_instructions_delta" => &["/attachment/addedBlocks"],
        "hook_non_blocking_error" => &["/attachment/stderr", "/attachment/stdout"],
        "queued_command" => &["/attachment/prompt", "/attachment/prompt/0/text"],
        "read_truncation_notice" => &["/attachment/banner"],
        _ => &[],
    }
}

/// The attachment's scalar members as rows, in the order the harness
/// wrote them, without its type, the members its subtype is made of,
/// and the members shown as text.
fn attachment_scalars(entry: &EntryValue, text_pointers: &[&str]) -> Vec<BodyAttr> {
    const SHOWN: [&str; 4] = ["type", "reminderType", "hookEvent", "hookName"];
    let members = entry.pointer("/attachment").and_then(EntryValue::as_object);
    members
        .into_iter()
        .flatten()
        .filter(|(key, _)| !SHOWN.contains(&key.as_str()))
        .filter(|(key, _)| !text_pointers.iter().any(|p| caption(p) == key.as_str()))
        .filter_map(|(key, value)| Some(BodyAttr::new(key, scalar(value)?)))
        .collect()
}

/// The session's totals, then each model's usage under the model's
/// name.
fn cost_state(entry: &EntryValue) -> Vec<BodyBlock> {
    let totals: Vec<BodyAttr> = scalar_members(entry)
        .into_iter()
        .filter(|attr| !["type", "sessionId"].contains(&attr.caption.as_str()))
        .collect();
    let mut blocks = rows_then_text(totals, Vec::new());
    let models = entry.pointer("/modelUsage").and_then(EntryValue::as_object);
    for (model, usage) in models.into_iter().flatten() {
        blocks.push(BodyBlock::Heading(model.clone()));
        blocks.push(BodyBlock::Attrs(scalar_members(usage)));
    }
    blocks
}

/// The files the snapshot tracks, one per line.
fn file_history_snapshot(entry: &EntryValue) -> Vec<BodyBlock> {
    let files = entry
        .pointer("/snapshot/trackedFileBackups")
        .and_then(EntryValue::as_object);
    let paths: Vec<&str> = files
        .into_iter()
        .flatten()
        .map(|(path, _)| path.as_str())
        .collect();
    if paths.is_empty() {
        return vec![BodyBlock::Remark("No files".to_string())];
    }
    vec![BodyBlock::Text(paths.join("\n"))]
}

/// The role of the request message the harness recorded, then the
/// message's text: for a user or assistant message the text the
/// `message` table would hold, for any other role its text blocks.
fn api_request_blob(entry: &EntryValue) -> Vec<BodyBlock> {
    let role = str_at(entry, "/message/role").unwrap_or_default();
    let text = match (role, entry.pointer("/message")) {
        ("user" | "assistant", Some(message)) => {
            let as_entry = serde_json::json!({"type": role, "message": message.to_json_value()});
            entry_text(&as_entry).into_iter().collect()
        }
        _ => {
            let blocks: Vec<&str> = entry
                .pointer("/message/content")
                .and_then(EntryValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(|block| str_at(block, "/text"))
                .collect();
            let mut text = texts(entry, &["/message/content"]);
            if !blocks.is_empty() {
                text.push(blocks.join("\n\n"));
            }
            text
        }
    };
    rows_then_text(attrs(entry, &["/message/role"]), text)
}

/// One row per artifact the harness watches for comments: its title
/// and the watcher's state.
fn artifact_comment_monitor(entry: &EntryValue) -> Vec<BodyBlock> {
    let artifacts = entry.pointer("/artifacts").and_then(EntryValue::as_object);
    let rows: Vec<BodyAttr> = artifacts
        .into_iter()
        .flatten()
        .map(|(id, artifact)| {
            BodyAttr::new(
                str_at(artifact, "/title").unwrap_or(id),
                str_at(artifact, "/state").unwrap_or_default(),
            )
        })
        .collect();
    rows_then_text(rows, Vec::new())
}

/// The ids of the artifacts the entry is about, one per line.
fn artifact_ids(entry: &EntryValue) -> Vec<BodyBlock> {
    let artifacts = entry.pointer("/artifacts").and_then(EntryValue::as_object);
    let ids: Vec<&str> = artifacts
        .into_iter()
        .flatten()
        .map(|(id, _)| id.as_str())
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    vec![BodyBlock::Text(ids.join("\n"))]
}

/// The pointers to the rows and to the text of an entry type whose
/// body is values read straight from the entry. A type without an arm
/// has no body.
fn selection(kind: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        "ai-title" => (&[], &["/aiTitle"]),
        "custom-title" => (&[], &["/customTitle"]),
        "agent-name" => (&[], &["/agentName"]),
        "last-prompt" => (&["/leafUuid"], &["/lastPrompt"]),
        "mode" => (&["/mode"], &[]),
        "permission-mode" => (&["/permissionMode"], &[]),
        "tools-used" => (&[], &["/tools"]),
        "queue-operation" => (&["/operation", "/reason"], &["/content"]),
        "atis-latch" => (&["/atis"], &[]),
        "session-owner" => (&["/accountUuid", "/organizationUuid"], &[]),
        "bridge-session" => (
            &[
                "/bridgeSessionId",
                "/lastSequenceNum",
                "/ownerAccountUuid",
                "/ownerOrganizationUuid",
            ],
            &[],
        ),
        "file-history-delta" => (
            &["/trackingPath", "/backup/version", "/backup/backupTime"],
            &[],
        ),
        "api-request" => (
            &[
                "/version",
                "/querySource",
                "/params/model",
                "/params/max_tokens",
            ],
            &[],
        ),
        "api-request-shape" => (&["/version"], &[]),
        "frame-link" => (&["/title", "/frameUrl", "/path", "/artifactCount"], &[]),
        _ => (&[], &[]),
    }
}

/// The attribute block when it has rows, then one text block per text.
fn rows_then_text(rows: Vec<BodyAttr>, text: Vec<String>) -> Vec<BodyBlock> {
    let mut blocks = Vec::new();
    if !rows.is_empty() {
        blocks.push(BodyBlock::Attrs(rows));
    }
    blocks.extend(text.into_iter().map(BodyBlock::Text));
    blocks
}

/// One row per pointer that selects a scalar, captioned by the
/// pointer's last segment.
fn attrs(entry: &EntryValue, pointers: &[&str]) -> Vec<BodyAttr> {
    pointers
        .iter()
        .filter_map(|pointer| {
            let value = scalar(entry.pointer(pointer)?)?;
            Some(BodyAttr::new(caption(pointer), value))
        })
        .collect()
}

/// One row per scalar member of `value`, in the order the harness
/// wrote them.
fn scalar_members(value: &EntryValue) -> Vec<BodyAttr> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, member)| Some(BodyAttr::new(key, scalar(member)?)))
        .collect()
}

fn caption(pointer: &str) -> &str {
    pointer.rsplit_once('/').map_or(pointer, |(_, last)| last)
}

/// A non-empty string, a number, or a boolean as text; nothing for any
/// other value.
fn scalar(value: &EntryValue) -> Option<String> {
    match value {
        EntryValue::String(s) if !s.is_empty() => Some(s.clone()),
        EntryValue::Number(n) => Some(n.to_string()),
        EntryValue::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The trimmed, non-empty strings each pointer selects: a string, or
/// the elements of an array whose elements are all strings. A pointer
/// that selects any other value selects nothing.
fn texts(entry: &EntryValue, pointers: &[&str]) -> Vec<String> {
    pointers
        .iter()
        .filter_map(|pointer| entry.pointer(pointer))
        .flat_map(|value| match value {
            EntryValue::String(s) => vec![s.as_str()],
            EntryValue::Array(items) => items
                .iter()
                .map(EntryValue::as_str)
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default(),
            _ => Vec::new(),
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn str_at<'a>(value: &'a EntryValue, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer)?.as_str()
}

#[cfg(test)]
mod tests {
    use gage_session::EntryCodec;
    use serde_json::json;

    use super::*;
    use crate::codec::codec;

    /// The body of the entry `text` decodes to, through the codec, so
    /// the members keep the order they are written in.
    fn body(text: &str) -> Vec<BodyBlock> {
        let entry = codec().decode(text.as_bytes()).unwrap();
        entry_body(&entry).blocks
    }

    fn body_of(value: serde_json::Value) -> Vec<BodyBlock> {
        body(&value.to_string())
    }

    fn rows(pairs: &[(&str, &str)]) -> BodyBlock {
        BodyBlock::Attrs(pairs.iter().map(|(c, v)| BodyAttr::new(*c, *v)).collect())
    }

    fn text(s: &str) -> BodyBlock {
        BodyBlock::Text(s.to_string())
    }

    fn remark(s: &str) -> BodyBlock {
        BodyBlock::Remark(s.to_string())
    }

    /// A message with text is that text as markdown, without the tags
    /// split off the front of a user message.
    #[test]
    fn a_message_is_its_text() {
        let blocks = body_of(json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "text", "text": "<ide_opened_file>a.rs</ide_opened_file>"},
                {"type": "text", "text": "hello **there**"},
            ]},
        }));
        assert_eq!(blocks, [BodyBlock::Markdown("hello **there**".to_string())]);

        let blocks = body_of(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": "- one"}]},
        }));
        assert_eq!(blocks, [BodyBlock::Markdown("- one".to_string())]);
    }

    /// A user message that is all tags shows each top-level tag as a
    /// row; a nested tag and an attribute on the open tag stay out of
    /// the name.
    #[test]
    fn a_message_of_tags_alone_is_its_tags() {
        let blocks = body_of(json!({
            "type": "user",
            "message": {"role": "user", "content": concat!(
                "<command-message>gage:publish</command-message>\n",
                "<command-name>/gage:publish</command-name>\n",
                "<task-notification>\n<task-id>t1</task-id>\n</task-notification>",
            )},
        }));
        assert_eq!(
            blocks,
            [rows(&[
                ("command-message", "gage:publish"),
                ("command-name", "/gage:publish"),
                ("task-notification", "<task-id>t1</task-id>"),
            ])]
        );
    }

    /// Tag text that does not open a closed pair is kept whole under
    /// an empty name, after the pairs before it.
    #[test]
    fn tag_text_that_is_not_a_pair_is_kept() {
        assert_eq!(
            tag_pairs("<command-name>/x</command-name> <unclosed>rest"),
            [
                BodyAttr::new("command-name", "/x"),
                BodyAttr::new("", "<unclosed>rest"),
            ]
        );
        assert_eq!(
            tag_pairs("<ide_selection lines=\"1-2\">fn a() {}</ide_selection>"),
            [BodyAttr::new("ide_selection", "fn a() {}")]
        );
        assert_eq!(tag_pairs("  "), []);
    }

    /// A message with no text and no tags says that it is empty: a
    /// thinking block that holds a signature alone, and a user message
    /// with empty content.
    #[test]
    fn a_message_without_content_is_marked_empty() {
        let thinking = body_of(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": "abc"},
            ]},
        }));
        assert_eq!(thinking, [remark("Empty")]);

        let user = body_of(json!({"type": "user", "message": {"role": "user", "content": ""}}));
        assert_eq!(user, [remark("Empty")]);
    }

    /// A message that is an image alone shows the size the image's
    /// base64 text decodes to.
    #[test]
    fn an_image_message_shows_the_image_size() {
        let blocks = body_of(json!({
            "type": "user",
            "isMeta": true,
            "message": {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "data": "A".repeat(4000)}},
            ]},
        }));
        assert_eq!(blocks, [remark("Image (3.0 kB)")]);
    }

    /// A tool result message is the result's text as written, which
    /// the viewer shows for a result the `tool_use` table has no row
    /// for.
    #[test]
    fn a_tool_result_message_is_the_result_text() {
        let blocks = body_of(json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [{"type": "text", "text": "Async agent launched"}],
            }]},
        }));
        assert_eq!(blocks, [text("Async agent launched")]);
    }

    /// A system entry with a message row is its text; a bookkeeping
    /// subtype shows its counts; any other subtype shows its content.
    #[test]
    fn a_system_entry_shows_its_text_or_its_bookkeeping() {
        let promoted = body_of(json!({
            "type": "system", "subtype": "informational", "content": "Unknown command",
        }));
        assert_eq!(
            promoted,
            [BodyBlock::Markdown("Unknown command".to_string())]
        );

        let duration = body(
            r#"{"type":"system","subtype":"turn_duration","durationMs":5352,"messageCount":19}"#,
        );
        assert_eq!(
            duration,
            [rows(&[("durationMs", "5352"), ("messageCount", "19")])]
        );

        let other = body_of(json!({
            "type": "system", "subtype": "bridge_status",
            "content": "/remote-control is active", "url": "https://x",
        }));
        assert_eq!(
            other,
            [
                rows(&[("url", "https://x")]),
                text("/remote-control is active")
            ]
        );
    }

    /// An attachment with `rendered` text shows its type, its subtype,
    /// and each rendered item without the `<system-reminder>` wrapper.
    #[test]
    fn an_attachment_shows_its_type_and_rendered_text() {
        let blocks = body_of(json!({
            "type": "attachment",
            "attachment": {"type": "auto_mode", "reminderType": "bash_first", "other": "x"},
            "rendered": [
                {"content": "<system-reminder>\nWhile auto mode is active\n</system-reminder>"},
                {"content": [
                    {"type": "text", "text": "first"},
                    {"type": "text", "text": "second"},
                ]},
            ],
        }));
        assert_eq!(
            blocks,
            [
                rows(&[("Type", "auto_mode"), ("Subtype", "bash_first")]),
                text("While auto mode is active"),
                text("first\n\nsecond"),
            ]
        );
    }

    /// A hook attachment's subtype is its event and hook name.
    #[test]
    fn a_hook_attachment_subtype_is_its_event_and_name() {
        let blocks = body_of(json!({
            "type": "attachment",
            "attachment": {"type": "hook_cancelled", "hookEvent": "Stop", "hookName": "sound"},
        }));
        assert_eq!(
            blocks,
            [rows(&[
                ("Type", "hook_cancelled"),
                ("Subtype", "Stop sound")
            ])]
        );
    }

    /// An attachment without `rendered` text shows its scalar members
    /// as rows in the order they are written, and the member its type
    /// keeps text in as text. Nested members are not shown.
    #[test]
    fn an_attachment_without_rendered_text_shows_its_members() {
        let blocks = body(concat!(
            r#"{"type":"attachment","attachment":{"type":"prompt_snapshot","#,
            r##""systemPrompt":["\nYou are an agent.\n","# Harness"],"reminderFold":false,"##,
            r#""echoWireToolInputs":true,"contextRendering":"announced","nested":{"a":1}}}"#,
        ));
        assert_eq!(
            blocks,
            [
                rows(&[
                    ("Type", "prompt_snapshot"),
                    ("reminderFold", "false"),
                    ("echoWireToolInputs", "true"),
                    ("contextRendering", "announced"),
                ]),
                text("You are an agent."),
                text("# Harness"),
            ]
        );
    }

    /// An attachment with nothing a reader wants shows its type alone.
    #[test]
    fn an_attachment_with_no_scalar_members_shows_its_type() {
        for attachment in [
            json!({"type": "auto_mode_exit"}),
            json!({"type": "todo_reminder", "content": [{"content": "x", "status": "pending"}]}),
            json!({"type": "deferred_tools_record", "entries": [], "toolInputCopies": [{"id": "t"}]}),
        ] {
            let kind = attachment["type"].as_str().unwrap().to_string();
            let blocks = body_of(json!({"type": "attachment", "attachment": attachment}));
            assert_eq!(blocks, [rows(&[("Type", &kind)])]);
        }
    }

    /// A tool record with definitions lists the tools' names.
    #[test]
    fn a_deferred_tools_record_lists_its_tool_names() {
        let blocks = body_of(json!({
            "type": "attachment",
            "attachment": {"type": "deferred_tools_record", "entries": [
                {"name": "WebFetch", "description": "Fetches a URL", "input_schema": {}},
                {"name": "WebSearch", "description": "Searches", "input_schema": {}},
            ]},
        }));
        assert_eq!(
            blocks,
            [
                rows(&[("Type", "deferred_tools_record")]),
                text("WebFetch\nWebSearch")
            ]
        );
    }

    /// A batching reminder shows the reminder's text and not the
    /// members beside it; a thinking drop shows its counts and reason
    /// and none of its ids.
    #[test]
    fn peculiar_attachments_show_what_a_reader_wants() {
        let reminder = body_of(json!({
            "type": "attachment",
            "attachment": {
                "type": "batching_reminder_sent", "text": "List first.",
                "model": "claude-x", "clearAt": "next_user_message",
            },
        }));
        assert_eq!(
            reminder,
            [
                rows(&[("Type", "batching_reminder_sent")]),
                text("List first.")
            ]
        );

        let drop = body_of(json!({
            "type": "attachment",
            "attachment": {
                "type": "thinking_drop", "requestId": "req_1", "blockHashes": ["h"],
                "newlyDropped": {"blockCount": 2, "turnCount": 1, "reason": "prefix_mismatch"},
            },
        }));
        assert_eq!(
            drop,
            [rows(&[
                ("Type", "thinking_drop"),
                ("Dropped", "2 blocks in 1 turn"),
                ("Reason", "prefix_mismatch"),
            ])]
        );
    }

    /// A cost state shows the session totals, then each model's usage
    /// under a heading, all in the order they are written.
    #[test]
    fn a_cost_state_shows_totals_then_each_model() {
        let blocks = body(concat!(
            r#"{"type":"cost-state","sessionId":"s","totalCostUSD":0.5,"totalDuration":10977,"#,
            r#""modelUsage":{"claude-opus":{"outputTokens":22,"inputTokens":1213},"#,
            r#""claude-fable":{"inputTokens":34}},"hasUnknownModelCost":false}"#,
        ));
        assert_eq!(
            blocks,
            [
                rows(&[
                    ("totalCostUSD", "0.5"),
                    ("totalDuration", "10977"),
                    ("hasUnknownModelCost", "false"),
                ]),
                BodyBlock::Heading("claude-opus".to_string()),
                rows(&[("outputTokens", "22"), ("inputTokens", "1213")]),
                BodyBlock::Heading("claude-fable".to_string()),
                rows(&[("inputTokens", "34")]),
            ]
        );
    }

    /// A file history snapshot lists the files it tracks in the order
    /// they are written, and says so when it tracks none.
    #[test]
    fn a_file_history_snapshot_lists_its_files() {
        let blocks = body(concat!(
            r#"{"type":"file-history-snapshot","snapshot":{"trackedFileBackups":{"#,
            r#""src/runner.rs":{"version":8},"src/cmd.rs":{"version":2}}}}"#,
        ));
        assert_eq!(blocks, [text("src/runner.rs\nsrc/cmd.rs")]);

        let none = body_of(json!({
            "type": "file-history-snapshot", "snapshot": {"trackedFileBackups": {}},
        }));
        assert_eq!(none, [remark("No files")]);
    }

    /// A request blob shows the message's role, then its text: a
    /// string as it is, text blocks joined by an empty line, and a
    /// user or assistant message as the `message` table would hold it.
    #[test]
    fn an_api_request_blob_shows_its_role_and_text() {
        let string = body_of(json!({
            "type": "api-request-blob", "hash": "h",
            "message": {"role": "system", "content": "List first."},
        }));
        assert_eq!(string, [rows(&[("role", "system")]), text("List first.")]);

        let blocks = body_of(json!({
            "type": "api-request-blob",
            "message": {"role": "system", "content": [
                {"type": "text", "text": "one"}, {"type": "text", "text": "two"},
            ]},
        }));
        assert_eq!(blocks, [rows(&[("role", "system")]), text("one\n\ntwo")]);

        let user = body_of(json!({
            "type": "api-request-blob",
            "message": {"role": "user", "content": [
                {"type": "text", "text": "one"}, {"type": "text", "text": "two"},
            ]},
        }));
        assert_eq!(user, [rows(&[("role", "user")]), text("one\n\ntwo")]);
    }

    /// A comment monitor shows each artifact's title and state; an
    /// artifact without a title is named by its id.
    #[test]
    fn an_artifact_comment_monitor_shows_titles_and_states() {
        let blocks = body(concat!(
            r#"{"type":"artifact-comment-monitor","artifacts":{"#,
            r#""id-1":{"state":"stopped","title":"Codebook"},"id-2":{"state":"running"}}}"#,
        ));
        assert_eq!(
            blocks,
            [rows(&[("Codebook", "stopped"), ("id-2", "running")])]
        );
    }

    /// A type read by pointer shows its scalars as rows and its
    /// strings as text; a pointer that selects a value of another
    /// shape selects nothing.
    #[test]
    fn selected_types_show_rows_then_text() {
        let queued = body_of(json!({
            "type": "queue-operation", "operation": "remove",
            "reason": "absorbed_mid_turn", "content": "When you're done",
        }));
        assert_eq!(
            queued,
            [
                rows(&[("operation", "remove"), ("reason", "absorbed_mid_turn")]),
                text("When you're done"),
            ]
        );

        let tools = body_of(json!({"type": "tools-used", "tools": ["Read", "Edit"]}));
        assert_eq!(tools, [text("Read"), text("Edit")]);

        assert_eq!(
            body_of(json!({"type": "mode", "mode": {"nested": "x"}})),
            []
        );
        assert_eq!(
            body_of(json!({"type": "tools-used", "tools": ["a", 1]})),
            []
        );
    }

    /// An invalid entry shows the codec's error and the raw line.
    #[test]
    fn an_invalid_entry_shows_its_error_and_raw_line() {
        let entry = EntryValue::invalid_entry("expected `,`", "{\"type\":\"us");
        assert_eq!(
            entry_body(&entry).blocks,
            [rows(&[("Error", "expected `,`")]), text("{\"type\":\"us")]
        );
    }

    /// A type the driver has no body for, and a value without a type,
    /// have no blocks.
    #[test]
    fn an_unknown_type_has_no_body() {
        assert_eq!(body_of(json!({"type": "not-a-type", "content": "x"})), []);
        assert_eq!(body_of(json!({"content": "x"})), []);
    }
}
