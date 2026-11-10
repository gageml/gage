//! Document model for the TUI.
//!
//! `Document` owns the session content. `Session` and `Entry` both wrap a
//! parsed JSON value (the source of truth for rendering) and expose accessors
//! plus a `yaml()` serializer. The two follow the same pattern so the body
//! pane renders them through the same highlighter path.

use std::path::PathBuf;

use gage_session::INVALID_ENTRY_TYPE;
use gage_store::{ChangeKind, NoteValue};
use serde_json::Value;

use crate::selection::{Selection, select};

pub struct Document {
    pub session: Session,
    pub entries: Vec<Entry>,
    /// The turn of each entry, aligned with `entries`: the assistant
    /// rows of one API response share a turn, numbered in order of
    /// first appearance; every other row has none
    pub turns: Vec<Option<usize>>,
    /// The session's tool calls, in the order the `tool_use` table
    /// yields them, which is the order of the calls in the session
    pub tool_uses: Vec<ToolUse>,
    pub notes: Vec<Note>,
    /// The store attachments that target the session, oldest first
    pub attachments: Vec<SessionAttachment>,
    /// The records of a published session's `changes.json`, in
    /// original line order; empty for a session that is not a publish
    pub redactions: Vec<Redaction>,
}

/// One record of a published session's `changes.json`. Its lines
/// are the original session's; [`Document::original_line`] maps an
/// entry of the derived session back to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub kind: ChangeKind,
    pub line: u32,
    pub line_end: Option<u32>,
    pub reference: Option<String>,
    pub span_start: Option<u64>,
    pub span_end: Option<u64>,
    /// The reason code; a `Title` record has none
    pub reason: Option<String>,
    pub author: String,
}

impl Redaction {
    /// The original lines a `Line` record dropped, inclusive.
    fn dropped_lines(&self) -> Option<(u32, u32)> {
        match self.kind {
            ChangeKind::Entry => Some((self.line, self.line_end.unwrap_or(self.line))),
            ChangeKind::Value | ChangeKind::Text | ChangeKind::Title => None,
        }
    }
}

/// A run of original lines the publish dropped, and where the run
/// sits among the derived session's entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub line: u32,
    pub line_end: u32,
    /// The index of the first entry after the run; `entries.len()`
    /// when the run follows the last entry
    pub anchor: usize,
}

impl Document {
    /// The runs of dropped original lines, merged where `Line` records
    /// overlap or touch, in line order, each anchored to the entry
    /// that follows it.
    pub fn dropped(&self) -> Vec<Dropped> {
        let mut ranges: Vec<(u32, u32)> = self
            .redactions
            .iter()
            .filter_map(Redaction::dropped_lines)
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (line, line_end) in ranges {
            match merged.last_mut() {
                Some(last) if line <= last.1.saturating_add(1) => last.1 = last.1.max(line_end),
                _ => merged.push((line, line_end)),
            }
        }
        // A dropped run shifts every later entry's original line by
        // its length, so the entries' original lines follow from the
        // runs before them.
        let mut dropped = Vec::with_capacity(merged.len());
        let mut entry = 0;
        let mut shift: u32 = 0;
        for (line, line_end) in merged {
            while self
                .entries
                .get(entry)
                .is_some_and(|e| e.line + shift < line)
            {
                entry += 1;
            }
            dropped.push(Dropped {
                line,
                line_end,
                anchor: entry,
            });
            shift += line_end - line + 1;
        }
        dropped
    }

    /// The original session's line of entry `index`: its derived line
    /// plus the dropped lines before it.
    pub fn original_line(&self, index: usize) -> Option<u32> {
        let line = self.entries.get(index)?.line;
        let shift: u32 = self
            .dropped()
            .iter()
            .take_while(|d| d.anchor <= index)
            .map(|d| d.line_end - d.line + 1)
            .sum();
        Some(line + shift)
    }

    /// The `Text` records on entry `index`, matched by its original
    /// line, and the `Title` record of an entry the publish added,
    /// matched by its derived line, in record order.
    pub fn entry_redactions(&self, index: usize) -> Vec<&Redaction> {
        let Some(entry) = self.entries.get(index) else {
            return Vec::new();
        };
        let Some(line) = self.original_line(index) else {
            return Vec::new();
        };
        self.redactions
            .iter()
            .filter(|r| match r.kind {
                ChangeKind::Text => r.line == line,
                ChangeKind::Title => r.line == entry.line,
                ChangeKind::Entry | ChangeKind::Value => false,
            })
            .collect()
    }

    /// The `Line` records that cover any line of `dropped`, in record
    /// order.
    pub fn dropped_redactions(&self, dropped: &Dropped) -> Vec<&Redaction> {
        self.redactions
            .iter()
            .filter(|r| {
                r.dropped_lines()
                    .is_some_and(|(line, end)| line <= dropped.line_end && end >= dropped.line)
            })
            .collect()
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|n| n.id == id)
    }

    pub fn attachment(&self, id: &str) -> Option<&SessionAttachment> {
        self.attachments.iter().find(|a| a.id == id)
    }

    /// Notes anchored at a specific line. A range target anchors at
    /// its first line; the range's extent shows in the note body's
    /// header instead.
    pub fn notes_for_line(&self, line: u32) -> Vec<&Note> {
        self.notes.iter().filter(|n| n.line == Some(line)).collect()
    }

    /// Notes attached to the session itself — a session target with no line.
    pub fn session_notes(&self) -> Vec<&Note> {
        self.notes.iter().filter(|n| n.line.is_none()).collect()
    }

    /// Index of the entry at JSONL line `line`, or of the nearest entry
    /// before it when that line is not among the document's entries.
    pub fn entry_index_for_line(&self, line: u32) -> Option<usize> {
        self.entries.iter().rposition(|e| e.line <= line)
    }

    pub fn add_note(&mut self, note: Note) {
        self.notes.push(note);
    }

    pub fn remove_note(&mut self, id: &str) {
        self.notes.retain(|n| n.id != id);
    }

    pub fn replace_note_value(&mut self, id: &str, value: NoteValue, modified_ms: i64) {
        if let Some(n) = self.notes.iter_mut().find(|n| n.id == id) {
            n.value = value;
            n.modified_ms = modified_ms;
        }
    }
}

/// A note on the document's session, as the viewer shows it. The
/// target's line selection is reduced to its first range: `line` is
/// where the note anchors in the outline and `line_end` the range's
/// last line when it spans more than one.
#[derive(Debug, Clone)]
pub struct Note {
    pub id: String,
    pub name: String,
    pub value: NoteValue,
    /// The writer's Gage URL, `user:<name>` for a person
    pub author: String,
    /// The anchor line; `None` for a note on the whole session
    pub line: Option<u32>,
    pub line_end: Option<u32>,
    pub metadata: Option<Value>,
    pub created_ms: i64,
    pub modified_ms: i64,
}

impl Note {
    /// The line count of the target's range, when it has one. A
    /// single-line or whole-session target yields `None`.
    pub fn span_lines(&self) -> Option<u32> {
        match (self.line, self.line_end) {
            (Some(line), Some(end)) if end > line => Some(end - line + 1),
            _ => None,
        }
    }

    /// The value as text: the string of a text value, otherwise the
    /// JSON form.
    pub fn text(&self) -> String {
        match &self.value {
            NoteValue::Text(s) => s.clone(),
            NoteValue::Json(v) => v.to_string(),
        }
    }
}

pub struct Session {
    pub id: String,
    pub value: Value,
    /// A pending publish working tree rather than a stored object; a
    /// note cannot target it
    pub pending: bool,
    pub attrs: SessionAttrs,
}

/// The session's attributes as the session listing shows them, each
/// formatted for display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionAttrs {
    /// The title; empty when the session has none
    pub title: String,
    /// Label and value pairs in listing order: Project, Type for a
    /// stored session or Driver for a native one, Model, Size,
    /// Messages, and Created for a stored session or Modified for a
    /// native one
    pub fields: Vec<(&'static str, String)>,
}

pub struct Entry {
    pub line: u32,
    /// The harness's id for the entry, from the `entry` table
    pub uuid: Option<String>,
    /// The entry type, from the `entry` table
    pub entry_type: String,
    /// The message subtype the driver derived, from the `message`
    /// table; `None` for an entry that is not a message
    pub message_subtype: Option<String>,
    /// The message's text, from the `message` table; `None` for an
    /// entry that is not a message
    pub message_text: Option<String>,
    /// The tags the driver split off the front of a user message's
    /// text, from the `message` table, as the harness wrote them
    pub ide_tags: Option<String>,
    /// The harness's row, verbatim
    pub value: Value,
}

/// What the rendered view shows for an entry.
pub enum EntryBody {
    /// The message text, rendered as markdown, and the name and
    /// content of each tag split off the front of it, which are shown
    /// when there is no text
    Text {
        text: String,
        tags: Vec<(String, String)>,
    },
    /// The calls a tool call entry made
    ToolCalls(Vec<ToolUse>),
    /// The results a tool result entry returned
    ToolResults(Vec<ToolUse>),
    /// The content the harness injected beside a turn
    Attachment(Attachment),
    /// A native line the driver's codec could not decode, as the
    /// codec stands in for it
    InvalidEntry(InvalidEntry),
    /// The values the entry's selection rule names, for an entry no
    /// other body covers
    Selection(Selection),
}

/// The entry the codec writes in place of a native line it could not
/// decode: why, and the line's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidEntry {
    pub error: String,
    pub raw: String,
}

/// An attachment entry as the viewer shows it, read from the entry's
/// own row: the harness's `attachment.type`, a subtype a few types
/// carry beside it, and the text the harness sent to the model. A row
/// without that text shows what its selection rule names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub kind: String,
    /// `reminderType` where present, else the hook event and name
    pub subtype: Option<String>,
    /// Caption and value rows a selection rule names; empty for a row
    /// with `rendered` text
    pub attrs: Vec<(&'static str, String)>,
    /// The text of each `rendered` item in order, without the
    /// `<system-reminder>` wrapper the harness puts around it. A row
    /// without that text, one the harness never sent or one written
    /// before the harness recorded `rendered`, has the text its
    /// selection rule names, which may be none
    pub rendered: Vec<String>,
}

/// A store attachment that targets the document's session, as the
/// viewer shows it under the session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachment {
    pub id: String,
    pub name: Option<String>,
    pub key: Option<String>,
    /// Gage URLs of the objects the files are about
    pub targets: Vec<String>,
    /// The directory the files were selected under, on the machine
    /// that added them
    pub root: PathBuf,
    pub file_count: i64,
    /// Total bytes across the files
    pub size: i64,
    pub created_ms: i64,
    pub modified_ms: i64,
    /// The files in tree order
    pub files: Vec<AttachmentFile>,
}

impl SessionAttachment {
    pub fn file(&self, path: &str) -> Option<&AttachmentFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

/// One file of a session attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    /// The path relative to the attachment's root, `/`-separated
    pub path: String,
    pub size: i64,
    /// The content as UTF-8 text; `None` when the bytes are not UTF-8
    pub text: Option<String>,
}

/// One row of the `tool_use` table, as the viewer shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUse {
    /// Line of the entry that made the call
    pub line: u32,
    /// Line of the entry that returned the result, when one did
    pub result_line: Option<u32>,
    pub name: String,
    /// The call input, JSON as the harness recorded it
    pub input: String,
    /// The result content as text
    pub output: Option<String>,
    pub is_error: Option<bool>,
}

impl Document {
    /// The rendered view's content for the entry at `index`: its tool
    /// calls for a tool call row, its results for a tool result row,
    /// its attachment for an attachment row, its message text and
    /// leading tags for any other message, what its selection rule
    /// names for an entry with neither, nothing when no rule selects
    /// a value or for a tool row the `tool_use` table has no rows for.
    pub fn entry_body(&self, index: usize) -> Option<EntryBody> {
        let entry = self.entries.get(index)?;
        if entry.entry_type == "attachment" {
            return Some(EntryBody::Attachment(attachment_body(&entry.value)));
        }
        if entry.entry_type == INVALID_ENTRY_TYPE {
            return Some(EntryBody::InvalidEntry(invalid_entry_body(&entry.value)));
        }
        match entry.message_subtype.as_deref() {
            Some("tool_use") => {
                let calls: Vec<ToolUse> = self
                    .tool_uses
                    .iter()
                    .filter(|t| t.line == entry.line)
                    .cloned()
                    .collect();
                (!calls.is_empty()).then_some(EntryBody::ToolCalls(calls))
            }
            Some("tool_result") => {
                let results: Vec<ToolUse> = self
                    .tool_uses
                    .iter()
                    .filter(|t| t.result_line == Some(entry.line))
                    .cloned()
                    .collect();
                (!results.is_empty()).then_some(EntryBody::ToolResults(results))
            }
            _ => {
                let tags = entry.ide_tags.as_deref().map(tag_pairs).unwrap_or_default();
                match &entry.message_text {
                    Some(text) => Some(EntryBody::Text {
                        text: text.clone(),
                        tags,
                    }),
                    None if !tags.is_empty() => Some(EntryBody::Text {
                        text: String::new(),
                        tags,
                    }),
                    None => select(&entry.value).map(EntryBody::Selection),
                }
            }
        }
    }
}

/// The name and trimmed content of each top-level `<name>…</name>`
/// pair in `tags`, in order. A tag nested in a pair's content stays in
/// the content. Text that does not open a closed pair ends the walk
/// and is kept whole under an empty name, so nothing is left out.
fn tag_pairs(tags: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = tags.trim();
    while !rest.is_empty() {
        let Some((name, content, after)) = leading_pair(rest) else {
            pairs.push((String::new(), rest.to_string()));
            break;
        };
        pairs.push((name.to_string(), content.trim().to_string()));
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

/// The error and raw text of an invalid entry row; each is empty when
/// the row lacks it.
fn invalid_entry_body(value: &Value) -> InvalidEntry {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    InvalidEntry {
        error: field("error"),
        raw: field("raw"),
    }
}

/// The attachment an entry row carries. The viewer reads it from the
/// row because no table normalizes attachments; the `message` table
/// has no row for one.
fn attachment_body(value: &Value) -> Attachment {
    let attachment = value.get("attachment");
    let field = |key: &str| {
        attachment
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let subtype = field("reminderType").or_else(|| match (field("hookEvent"), field("hookName")) {
        (Some(event), Some(name)) => Some(format!("{event} {name}")),
        (event, name) => event.or(name),
    });
    let rendered: Vec<String> = value
        .get("rendered")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(rendered_text).collect())
        .unwrap_or_default();
    let (attrs, rendered) = if rendered.is_empty() {
        let selection = select(value).unwrap_or_default();
        (selection.attrs, selection.text)
    } else {
        (Vec::new(), rendered)
    };
    Attachment {
        kind: field("type").unwrap_or_default(),
        subtype,
        attrs,
        rendered,
    }
}

/// The text of one `rendered` item. Its `content` is a string, or on a
/// queued command a list of text blocks, which are joined. One outer
/// `<system-reminder>` wrapper is removed; the harness puts it around
/// every item, and a reader of the body knows the text is injected.
fn rendered_text(item: &Value) -> Option<String> {
    let text = match item.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
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
}

impl Entry {
    pub fn entry_type(&self) -> &str {
        &self.entry_type
    }

    /// Outline label — the message subtype when meaningful (e.g.
    /// `tool_use`, `thinking`, `tool_result`, `meta`), otherwise the
    /// entry type.
    pub fn label(&self) -> &str {
        match self.message_subtype.as_deref() {
            Some("text") | None => &self.entry_type,
            Some(sub) => sub,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn redaction(kind: ChangeKind, line: u32, line_end: Option<u32>) -> Redaction {
        Redaction {
            kind,
            line,
            line_end,
            reference: (kind != ChangeKind::Entry).then(|| "/message/content".to_string()),
            span_start: (kind == ChangeKind::Text).then_some(0),
            span_end: (kind == ChangeKind::Text).then_some(10),
            reason: (kind != ChangeKind::Title).then(|| "pii".to_string()),
            author: "filter:pii".to_string(),
        }
    }

    /// A derived session of five entries from an original of ten:
    /// lines 2-3 and 6 were dropped, as were lines 9-10 after the
    /// last kept entry, and the fifth entry is the title the publish
    /// added.
    fn derived() -> Document {
        Document {
            session: Session {
                id: "s1".to_string(),
                value: json!({}),
                pending: false,
                attrs: SessionAttrs::default(),
            },
            entries: (1..=5)
                .map(|line| Entry {
                    line,
                    uuid: None,
                    entry_type: "user".to_string(),
                    message_subtype: None,
                    message_text: None,
                    ide_tags: None,
                    value: json!({}),
                })
                .collect(),
            turns: Vec::new(),
            tool_uses: Vec::new(),
            notes: Vec::new(),
            attachments: Vec::new(),
            redactions: vec![
                redaction(ChangeKind::Text, 1, None),
                redaction(ChangeKind::Entry, 2, Some(3)),
                redaction(ChangeKind::Entry, 3, Some(3)),
                redaction(ChangeKind::Text, 5, None),
                redaction(ChangeKind::Entry, 6, None),
                redaction(ChangeKind::Entry, 9, Some(10)),
                redaction(ChangeKind::Value, 0, None),
                redaction(ChangeKind::Title, 5, None),
            ],
        }
    }

    /// Overlapping drops merge into one run, and each run is anchored
    /// to the first kept entry after it; a run past the last entry is
    /// anchored past the end.
    #[test]
    fn dropped_runs_merge_and_anchor_to_the_following_entry() {
        let doc = derived();
        let runs: Vec<(u32, u32, usize)> = doc
            .dropped()
            .iter()
            .map(|d| (d.line, d.line_end, d.anchor))
            .collect();
        assert_eq!(runs, [(2, 3, 1), (6, 6, 3), (9, 10, 5)]);
    }

    /// An entry's original line is its derived line shifted by every
    /// run before it.
    #[test]
    fn original_line_skips_the_dropped_runs() {
        let doc = derived();
        let originals: Vec<u32> = (0..5).map(|i| doc.original_line(i).unwrap()).collect();
        assert_eq!(originals, [1, 4, 5, 7, 8]);
        assert_eq!(doc.original_line(5), None);
    }

    /// Text records are matched to entries by original line, a title
    /// record by the derived line of the entry it added, a value or
    /// attr rule is on no entry, and a run lists every line record
    /// that touches it.
    #[test]
    fn records_are_found_by_original_line() {
        let doc = derived();
        let kinds = |index: usize| -> Vec<ChangeKind> {
            doc.entry_redactions(index).iter().map(|r| r.kind).collect()
        };
        assert_eq!(kinds(0), [ChangeKind::Text]);
        assert_eq!(kinds(1), []);
        assert_eq!(kinds(2), [ChangeKind::Text]);
        assert_eq!(kinds(3), []);
        assert_eq!(kinds(4), [ChangeKind::Title]);
        let first_run = &doc.dropped()[0];
        let lines: Vec<(u32, Option<u32>)> = doc
            .dropped_redactions(first_run)
            .iter()
            .map(|r| (r.line, r.line_end))
            .collect();
        assert_eq!(lines, [(2, Some(3)), (3, Some(3))]);
    }

    fn attachment_doc(value: Value) -> Document {
        let mut doc = derived();
        doc.entries[0].entry_type = "attachment".to_string();
        doc.entries[0].value = value;
        doc
    }

    /// An invalid entry row's body is the codec's error and the raw
    /// line.
    #[test]
    fn an_invalid_entry_row_shows_its_error_and_raw_line() {
        let mut doc = derived();
        doc.entries[0].entry_type = INVALID_ENTRY_TYPE.to_string();
        doc.entries[0].value = json!({
            "type": INVALID_ENTRY_TYPE,
            "error": "expected `,` or `}` at line 1 column 30",
            "raw": "{\"type\":\"user\",\"x\":\"cu{\"type\":\"c\"}",
        });
        let body = match doc.entry_body(0) {
            Some(EntryBody::InvalidEntry(body)) => body,
            _ => panic!("an invalid entry row has an invalid entry body"),
        };
        assert_eq!(
            body,
            InvalidEntry {
                error: "expected `,` or `}` at line 1 column 30".to_string(),
                raw: "{\"type\":\"user\",\"x\":\"cu{\"type\":\"c\"}".to_string(),
            }
        );
    }

    fn attachment(doc: &Document) -> Attachment {
        match doc.entry_body(0) {
            Some(EntryBody::Attachment(a)) => a,
            _ => panic!("an attachment row has an attachment body"),
        }
    }

    /// An attachment row's body is its `attachment.type` and the text
    /// of each `rendered` item in order, whatever its message subtype.
    /// The `<system-reminder>` wrapper is removed; text without one is
    /// kept as it is.
    #[test]
    fn an_attachment_row_shows_its_type_and_rendered_text() {
        let doc = attachment_doc(json!({
            "type": "attachment",
            "attachment": {"type": "file", "filename": "a.rs"},
            "rendered": [
                {"content": "<system-reminder>\nCalled the Read tool\n</system-reminder>"},
                {"content": "fn a() {}"},
            ],
        }));
        assert_eq!(
            attachment(&doc),
            Attachment {
                kind: "file".to_string(),
                subtype: None,
                attrs: vec![],
                rendered: vec!["Called the Read tool".to_string(), "fn a() {}".to_string()],
            }
        );
    }

    /// A queued command's rendered content is a list of text blocks;
    /// their texts are joined into one item.
    #[test]
    fn rendered_text_blocks_are_joined_into_one_item() {
        let doc = attachment_doc(json!({
            "type": "attachment",
            "attachment": {"type": "queued_command", "prompt": "stop"},
            "rendered": [{"content": [
                {"type": "text", "text": "<system-reminder>\nThe user sent a new message:\nstop"},
                {"type": "text", "text": "Do not proceed.\n</system-reminder>"},
            ]}],
        }));
        assert_eq!(
            attachment(&doc).rendered,
            ["The user sent a new message:\nstop\n\nDo not proceed."]
        );
    }

    /// A row without `rendered` text keeps its type and shows what
    /// its selection rule names; a row no rule selects from has its
    /// type alone.
    #[test]
    fn an_attachment_row_without_rendered_text_shows_its_selection() {
        let doc = attachment_doc(json!({
            "type": "attachment",
            "attachment": {"type": "credential_org", "organizationUuid": "o1"},
        }));
        assert_eq!(
            attachment(&doc),
            Attachment {
                kind: "credential_org".to_string(),
                subtype: None,
                attrs: vec![("organizationUuid", "o1".to_string())],
                rendered: Vec::new(),
            }
        );

        let doc = attachment_doc(json!({
            "type": "attachment",
            "attachment": {"type": "batching_reminder_sent", "text": "List first."},
        }));
        assert_eq!(attachment(&doc).rendered, ["List first."]);

        let doc = attachment_doc(json!({
            "type": "attachment",
            "attachment": {"type": "auto_mode_exit"},
        }));
        assert_eq!(
            attachment(&doc),
            Attachment {
                kind: "auto_mode_exit".to_string(),
                subtype: None,
                attrs: Vec::new(),
                rendered: Vec::new(),
            }
        );
    }

    /// Each top-level pair is a name and its trimmed content; a
    /// nested tag and an attribute on the open tag stay out of the
    /// name.
    #[test]
    fn tag_pairs_are_the_top_level_names_and_contents() {
        let pairs = tag_pairs(concat!(
            "<command-message>gage:publish</command-message>\n",
            "<command-name>/gage:publish</command-name>\n",
            "<task-notification>\n<task-id>t1</task-id>\n</task-notification>",
            "<ide_selection lines=\"1-2\">fn a() {}</ide_selection>",
        ));
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("command-message", "gage:publish"),
                ("command-name", "/gage:publish"),
                ("task-notification", "<task-id>t1</task-id>"),
                ("ide_selection", "fn a() {}"),
            ]
        );
    }

    /// Text that does not open a closed pair is kept whole under an
    /// empty name, after the pairs before it.
    #[test]
    fn tag_text_that_is_not_a_pair_is_kept() {
        let pairs = tag_pairs("<command-name>/x</command-name> <unclosed>rest");
        assert_eq!(
            pairs,
            [
                ("command-name".to_string(), "/x".to_string()),
                (String::new(), "<unclosed>rest".to_string()),
            ]
        );
        assert_eq!(tag_pairs("  "), []);
    }

    /// A message's body carries its text and its tags; a message of
    /// tags alone has a body with empty text.
    #[test]
    fn a_message_body_carries_its_tags() {
        let mut doc = derived();
        doc.entries[0].message_text = Some("hello".to_string());
        doc.entries[0].ide_tags = Some("<ide_opened_file>a.rs</ide_opened_file>".to_string());
        let expected = vec![("ide_opened_file".to_string(), "a.rs".to_string())];
        match doc.entry_body(0) {
            Some(EntryBody::Text { text, tags }) => {
                assert_eq!(text, "hello");
                assert_eq!(tags, expected);
            }
            _ => panic!("a message row has a text body"),
        }
        doc.entries[0].message_text = None;
        match doc.entry_body(0) {
            Some(EntryBody::Text { text, tags }) => {
                assert_eq!(text, "");
                assert_eq!(tags, expected);
            }
            _ => panic!("a message of tags alone has a text body"),
        }
    }

    /// An entry without a message shows what its selection rule
    /// names; a message keeps its text; a type without a rule has no
    /// body.
    #[test]
    fn an_entry_without_a_message_shows_its_selection() {
        let mut doc = derived();
        doc.entries[0].entry_type = "ai-title".to_string();
        doc.entries[0].message_subtype = None;
        doc.entries[0].message_text = None;
        doc.entries[0].value = json!({"type": "ai-title", "aiTitle": "Review documentation"});
        match doc.entry_body(0) {
            Some(EntryBody::Selection(selection)) => {
                assert_eq!(selection.text, ["Review documentation"]);
            }
            _ => panic!("an ai-title row has a selection body"),
        }

        doc.entries[0].message_text = Some("message text".to_string());
        assert!(
            matches!(doc.entry_body(0), Some(EntryBody::Text { text, .. }) if text == "message text")
        );

        doc.entries[0].message_text = None;
        doc.entries[0].value = json!({"type": "not-a-type", "content": "x"});
        assert!(doc.entry_body(0).is_none());
    }

    /// A reminder's `reminderType` qualifies its type; a hook row is
    /// qualified by its event and hook name.
    #[test]
    fn an_attachment_subtype_is_the_reminder_type_or_the_hook() {
        let reminder = attachment_doc(json!({
            "type": "attachment",
            "attachment": {"type": "auto_mode", "reminderType": "bash_first"},
        }));
        assert_eq!(attachment(&reminder).subtype.as_deref(), Some("bash_first"));
        let hook = attachment_doc(json!({
            "type": "attachment",
            "attachment": {
                "type": "hook_success",
                "hookEvent": "PostToolUse",
                "hookName": "PostToolUse:Edit",
            },
        }));
        assert_eq!(
            attachment(&hook).subtype.as_deref(),
            Some("PostToolUse PostToolUse:Edit")
        );
    }
}
