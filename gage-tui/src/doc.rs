//! Document model for the TUI.
//!
//! `Document` owns the session content. `Session` and `Entry` both wrap a
//! parsed JSON value (the source of truth for rendering) and expose accessors
//! plus a `yaml()` serializer. The two follow the same pattern so the body
//! pane renders them through the same highlighter path.

use gage_db::note::Note;
use gage_db::target::NoteTarget;
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

    /// Notes attached to a specific line — exact match only, never a range.
    pub fn notes_for_line(&self, line: u32) -> Vec<&Note> {
        self.notes
            .iter()
            .filter(|n| matches!(&n.target, NoteTarget::Session(t) if t.line == Some(line) && t.line_end.is_none()))
            .collect()
    }

    pub fn add_note(&mut self, note: Note) {
        self.notes.push(note);
    }

    pub fn remove_note(&mut self, id: &str) {
        self.notes.retain(|n| n.id != id);
    }

    pub fn replace_note_value(&mut self, id: &str, value: gage_db::note::NoteValue, modified: i64) {
        if let Some(n) = self.notes.iter_mut().find(|n| n.id == id) {
            n.value = value;
            n.modified = Some(modified);
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
    /// the labeling used by `gage eval view`.
    pub fn label(&self) -> &str {
        match gage_claude::entry::entry_subtype(&self.value) {
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
