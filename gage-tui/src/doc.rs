//! Document model for the TUI.
//!
//! `Document` owns the session content. `Session` and `Entry` both wrap a
//! parsed JSON value (the source of truth for rendering) and expose accessors
//! plus a `yaml()` serializer. The two follow the same pattern so the body
//! pane renders them through the same highlighter path.

use gage_store::{NoteValue, RedactedKind};
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
    pub notes: Vec<Note>,
    /// The records of a published session's `redacted.json`, in
    /// original line order; empty for a session that is not a publish
    pub redactions: Vec<Redaction>,
}

/// One record of a published session's `redacted.json`. Its lines
/// are the original session's; [`Document::original_line`] maps an
/// entry of the derived session back to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub kind: RedactedKind,
    pub line: u32,
    pub line_end: Option<u32>,
    pub property: Option<String>,
    pub span_start: Option<u64>,
    pub span_end: Option<u64>,
    pub reason: String,
    pub author: String,
}

impl Redaction {
    /// The original lines a `Line` record dropped, inclusive.
    fn dropped_lines(&self) -> Option<(u32, u32)> {
        match self.kind {
            RedactedKind::Line => Some((self.line, self.line_end.unwrap_or(self.line))),
            RedactedKind::Span | RedactedKind::Value | RedactedKind::Attr => None,
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

    /// The `Span` and `Value` records on entry `index`, in record
    /// order.
    pub fn entry_redactions(&self, index: usize) -> Vec<&Redaction> {
        let Some(line) = self.original_line(index) else {
            return Vec::new();
        };
        self.redactions
            .iter()
            .filter(|r| r.kind != RedactedKind::Line && r.line == line)
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
}

impl Session {
    pub fn yaml(&self) -> String {
        serde_yml::to_string(&self.value).expect("Value is always YAML serializable")
    }
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
    /// The harness's row, verbatim
    pub value: Value,
}

/// What the rendered view shows for an entry.
pub enum EntryBody {
    /// The message text, rendered as markdown
    Text(String),
    /// The calls a tool call entry made
    ToolCalls(Vec<ToolUse>),
    /// The results a tool result entry returned
    ToolResults(Vec<ToolUse>),
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
    /// its message text for any other message, nothing for an entry
    /// without a message or for a tool row the `tool_use` table has
    /// no rows for.
    pub fn entry_body(&self, index: usize) -> Option<EntryBody> {
        let entry = self.entries.get(index)?;
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
            _ => entry.message_text.clone().map(EntryBody::Text),
        }
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

    pub fn yaml(&self) -> String {
        serde_yml::to_string(&self.value).expect("Value is always YAML serializable")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn redaction(kind: RedactedKind, line: u32, line_end: Option<u32>) -> Redaction {
        Redaction {
            kind,
            line,
            line_end,
            property: (kind != RedactedKind::Line).then(|| "/message/content".to_string()),
            span_start: (kind == RedactedKind::Span).then_some(0),
            span_end: (kind == RedactedKind::Span).then_some(3),
            reason: "pii".to_string(),
            author: "filter:pii".to_string(),
        }
    }

    /// A derived session of five entries from an original of ten:
    /// lines 2-3 and 6 were dropped, as were lines 9-10 after the
    /// last kept entry.
    fn derived() -> Document {
        Document {
            session: Session {
                id: "s1".to_string(),
                value: json!({}),
                pending: false,
            },
            entries: (1..=5)
                .map(|line| Entry {
                    line,
                    uuid: None,
                    entry_type: "user".to_string(),
                    message_subtype: None,
                    message_text: None,
                    value: json!({}),
                })
                .collect(),
            turns: Vec::new(),
            tool_uses: Vec::new(),
            notes: Vec::new(),
            redactions: vec![
                redaction(RedactedKind::Span, 1, None),
                redaction(RedactedKind::Line, 2, Some(3)),
                redaction(RedactedKind::Line, 3, Some(3)),
                redaction(RedactedKind::Value, 5, None),
                redaction(RedactedKind::Line, 6, None),
                redaction(RedactedKind::Line, 9, Some(10)),
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

    /// Text records are matched to entries by original line, and a
    /// run lists every line record that touches it.
    #[test]
    fn records_are_found_by_original_line() {
        let doc = derived();
        let kinds = |index: usize| -> Vec<RedactedKind> {
            doc.entry_redactions(index).iter().map(|r| r.kind).collect()
        };
        assert_eq!(kinds(0), [RedactedKind::Span]);
        assert_eq!(kinds(1), []);
        assert_eq!(kinds(2), [RedactedKind::Value]);
        let first_run = &doc.dropped()[0];
        let lines: Vec<(u32, Option<u32>)> = doc
            .dropped_redactions(first_run)
            .iter()
            .map(|r| (r.line, r.line_end))
            .collect();
        assert_eq!(lines, [(2, Some(3)), (3, Some(3))]);
    }

    /// A nested object renders as an indented YAML mapping, not as
    /// one quoted scalar.
    #[test]
    fn session_yaml_nests_object_values() {
        let session = Session {
            id: "s1".to_string(),
            value: json!({"id": "s1", "summary": {"title": "T", "message_count": 2}}),
            pending: false,
        };
        let yaml = session.yaml();
        assert!(yaml.contains("\nsummary:\n"), "{yaml}");
        assert!(yaml.contains("\n  title: T\n"), "{yaml}");
        assert!(yaml.contains("\n  message_count: 2\n"), "{yaml}");
    }
}
