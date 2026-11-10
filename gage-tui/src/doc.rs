//! Document model for the TUI.
//!
//! `Document` owns the session content. `Session` and `Entry` both wrap a
//! parsed JSON value (the source of truth for rendering) and expose accessors
//! plus a `yaml()` serializer. The two follow the same pattern so the body
//! pane renders them through the same highlighter path.

use std::path::PathBuf;

use std::sync::Arc;

use gage_session::{Driver, DriverError, EntryBody, EntryCodec, EntryValue};
use gage_store::{ChangeKind, NoteValue};
use serde_json::Value;

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
    /// What describes an entry's body
    pub bodies: BodySource,
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
    /// The harness's row as the driver's codec decodes it
    pub value: EntryValue,
}

/// The driver that describes the document's entries, and the content
/// format it reads them in.
pub struct BodySource {
    driver: Arc<dyn Driver>,
    content_format: String,
}

impl BodySource {
    pub fn new(driver: Arc<dyn Driver>, content_format: impl Into<String>) -> Self {
        Self {
            driver,
            content_format: content_format.into(),
        }
    }

    /// The codec that decodes the document's entries.
    pub fn codec(&self) -> Result<Arc<dyn EntryCodec>, DriverError> {
        self.driver.codec(&self.content_format)
    }

    fn entry_body(&self, entry: &EntryValue) -> EntryBody {
        self.driver.entry_body(&self.content_format, entry)
    }

    /// The Claude Code driver over the format the fixtures are in.
    #[cfg(test)]
    pub(crate) fn claude() -> Self {
        let registry = gage_registry::driver::DriverRegistry::builtin();
        Self::new(
            registry.for_name("claude").unwrap(),
            crate::format::SUPPORTED,
        )
    }
}

/// What the rendered view shows for an entry.
pub enum Body {
    /// The calls a tool call entry made
    ToolCalls(Vec<ToolUse>),
    /// The results a tool result entry returned
    ToolResults(Vec<ToolUse>),
    /// The body the session's driver describes for the entry
    Entry(EntryBody),
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
    /// calls for a tool call row and its results for a tool result
    /// row, when the `tool_use` table has rows for it, else the body
    /// the session's driver describes. Nothing for an entry the
    /// driver describes no body for.
    pub fn entry_body(&self, index: usize) -> Option<Body> {
        let entry = self.entries.get(index)?;
        let tool_uses: Vec<ToolUse> = match entry.message_subtype.as_deref() {
            Some("tool_use") => self.tool_uses_where(|t| t.line == entry.line),
            Some("tool_result") => self.tool_uses_where(|t| t.result_line == Some(entry.line)),
            _ => Vec::new(),
        };
        match entry.message_subtype.as_deref() {
            Some("tool_use") if !tool_uses.is_empty() => Some(Body::ToolCalls(tool_uses)),
            Some("tool_result") if !tool_uses.is_empty() => Some(Body::ToolResults(tool_uses)),
            _ => {
                let body = self.bodies.entry_body(&entry.value);
                (!body.blocks.is_empty()).then_some(Body::Entry(body))
            }
        }
    }

    fn tool_uses_where(&self, keep: impl Fn(&ToolUse) -> bool) -> Vec<ToolUse> {
        self.tool_uses.iter().filter(|t| keep(t)).cloned().collect()
    }
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
    use gage_session::BodyBlock;
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
                    value: json!({}).into(),
                })
                .collect(),
            turns: Vec::new(),
            tool_uses: Vec::new(),
            bodies: BodySource::claude(),
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

    fn blocks(doc: &Document, index: usize) -> Vec<BodyBlock> {
        match doc.entry_body(index) {
            Some(Body::Entry(body)) => body.blocks,
            _ => panic!("entry {index} has a driver body"),
        }
    }

    /// An entry's body is what the session's driver describes for its
    /// value; an entry the driver describes nothing for has no body.
    #[test]
    fn an_entry_body_is_the_drivers() {
        let mut doc = derived();
        doc.entries[0].value = json!({
            "type": "user",
            "message": {"role": "user", "content": "hello"},
        })
        .into();
        assert_eq!(blocks(&doc, 0), [BodyBlock::Markdown("hello".to_string())]);

        doc.entries[1].value = json!({"type": "ai-title", "aiTitle": "Review"}).into();
        assert_eq!(blocks(&doc, 1), [BodyBlock::Text("Review".to_string())]);

        doc.entries[2].value = json!({"type": "not-a-type"}).into();
        assert!(doc.entry_body(2).is_none());
    }

    /// A tool call row and a tool result row show the `tool_use`
    /// table's rows for their line. A tool row the table has no rows
    /// for shows the driver's body.
    #[test]
    fn a_tool_row_shows_its_table_rows_or_the_drivers_body() {
        let mut doc = derived();
        doc.entries[0].message_subtype = Some("tool_use".to_string());
        doc.entries[1].message_subtype = Some("tool_result".to_string());
        doc.entries[1].value = json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "Async agent launched",
            }]},
        })
        .into();
        assert_eq!(
            blocks(&doc, 1),
            [BodyBlock::Text("Async agent launched".to_string())]
        );

        doc.tool_uses = vec![ToolUse {
            line: 1,
            result_line: Some(2),
            name: "Bash".to_string(),
            input: "{}".to_string(),
            output: Some("ok".to_string()),
            is_error: None,
        }];
        assert!(matches!(doc.entry_body(0), Some(Body::ToolCalls(calls)) if calls.len() == 1));
        assert!(matches!(doc.entry_body(1), Some(Body::ToolResults(r)) if r.len() == 1));
    }
}
