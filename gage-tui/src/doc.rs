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
}

impl Session {
    pub fn yaml(&self) -> String {
        serde_yml::to_string(&self.value).expect("Value is always YAML serializable")
    }
}

pub struct Entry {
    pub line: u32,
    pub value: Value,
}

impl Entry {
    pub fn entry_type(&self) -> &str {
        self.value.get("type").and_then(Value::as_str).unwrap_or("")
    }

    /// Outline label — the subtype when meaningful (e.g. `tool_use`,
    /// `thinking`, `tool_result`, `meta`), otherwise the raw type. Mirrors
    /// the labeling used by `gage test view`.
    pub fn label(&self) -> &str {
        match gage_claude::entry::message_subtype(&self.value) {
            Some("text") | None => self.entry_type(),
            Some(sub) => sub,
        }
    }

    pub fn message(&self) -> Option<&Value> {
        self.value.get("message")
    }

    pub fn yaml(&self) -> String {
        serde_yml::to_string(&self.value).expect("Value is always YAML serializable")
    }
}
