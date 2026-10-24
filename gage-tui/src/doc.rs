//! Document model for the TUI.
//!
//! `Document` owns the session content. `Session` and `Entry` both wrap a
//! parsed JSON value (the source of truth for rendering) and expose accessors
//! plus a `yaml()` serializer. The two follow the same pattern so the body
//! pane renders them through the same highlighter path.

use gage_store::NoteValue;
use serde_json::Value;

pub struct Document {
    pub session: Session,
    pub entries: Vec<Entry>,
    /// The turn of each entry, aligned with `entries`: the assistant
    /// rows of one API response share a turn, numbered in order of
    /// first appearance; every other row has none
    pub turns: Vec<Option<usize>>,
    pub notes: Vec<Note>,
}

impl Document {
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
    /// A tool call or tool result, rendered from the message value
    Tool(Value),
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

    pub fn message(&self) -> Option<&Value> {
        self.value.get("message")
    }

    /// The rendered view's content: the message text for a message
    /// that is not a tool call or result, the message value for a tool
    /// call or result, nothing for an entry without a message.
    pub fn body(&self) -> Option<EntryBody> {
        match self.message_subtype.as_deref() {
            Some("tool_use" | "tool_result") => self.message().cloned().map(EntryBody::Tool),
            _ => self.message_text.clone().map(EntryBody::Text),
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
