//! TUI shell — header / outline / body / footer. Tab toggles which pane is
//! active. In the outline: j/k/g/G/PgUp/PgDn navigate, Enter toggles
//! expansion, Right expands, Left collapses (or moves to parent when the
//! current row has no children to collapse). In the body: j/k/g/G/PgUp/PgDn
//! scroll. `n` opens a note dialog over the selected entry; `e` edits the
//! selected user comment note; `d` deletes the selected user-authored note.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use gage_db::note::{self, Note, NoteValue};
use gage_db::rusqlite::Connection;
use gage_db::target::{NoteTarget, SessionTarget};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::doc::Document;
use crate::options::ViewOptions;
use crate::outline::{CollapseOutcome, Outline, RowKind};
use crate::syntax::Highlighter;
use crate::textarea::TextArea;
use crate::{message, style};

pub fn run(
    terminal: &mut DefaultTerminal,
    doc: Document,
    options: &ViewOptions,
    db: &Connection,
) -> io::Result<()> {
    let turns = options.show_turns.then(|| compute_turns(&doc));
    let mut state = AppState::new(doc);
    loop {
        terminal.draw(|frame| draw(frame, &mut state, turns.as_deref()))?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Dialog input takes precedence over global keys.
            match &mut state.dialog {
                Dialog::AddNote { .. } | Dialog::EditNote { .. } => {
                    handle_note_dialog(&mut state, key, db);
                    continue;
                }
                Dialog::ConfirmCancel { .. } => {
                    handle_confirm_cancel(&mut state, key.code);
                    continue;
                }
                Dialog::ConfirmDelete { .. } => {
                    handle_confirm_delete(&mut state, key.code, db);
                    continue;
                }
                Dialog::None => {}
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Tab | KeyCode::BackTab => state.toggle_focus(),
                KeyCode::Down | KeyCode::Char('j') => match state.focus {
                    Focus::Outline => state.select_by(1),
                    Focus::Body => state.body_scroll_by(1),
                },
                KeyCode::Up | KeyCode::Char('k') => match state.focus {
                    Focus::Outline => state.select_by(-1),
                    Focus::Body => state.body_scroll_by(-1),
                },
                KeyCode::Char('g') => match state.focus {
                    Focus::Outline => state.select_first(),
                    Focus::Body => state.body_scroll_to_top(),
                },
                KeyCode::Char('G') => match state.focus {
                    Focus::Outline => state.select_last(),
                    Focus::Body => state.body_scroll_to_bottom(),
                },
                KeyCode::PageDown => match state.focus {
                    Focus::Outline => state.select_by(state.outline_page() as isize),
                    Focus::Body => state.body_scroll_by(state.body_page() as i32),
                },
                KeyCode::PageUp => match state.focus {
                    Focus::Outline => state.select_by(-(state.outline_page() as isize)),
                    Focus::Body => state.body_scroll_by(-(state.body_page() as i32)),
                },
                KeyCode::Enter if state.focus == Focus::Outline => {
                    if !state.begin_edit_note() {
                        state.toggle_selected();
                    }
                }
                KeyCode::Right if state.focus == Focus::Outline => state.expand_selected(),
                KeyCode::Left if state.focus == Focus::Outline => state.collapse_selected(),
                KeyCode::Char('n') => state.begin_add_note(),
                KeyCode::Char('e') => {
                    state.begin_edit_note();
                }
                KeyCode::Char('d') => state.begin_delete_note(),
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Outline,
    Body,
}

enum Dialog {
    None,
    AddNote {
        entry_index: usize,
        editor: TextArea,
    },
    EditNote {
        note_id: String,
        original: String,
        editor: TextArea,
    },
    /// Sub-dialog layered over the editor when the user pressed Esc with
    /// non-empty content. `y` discards `pending`; anything else restores it.
    ConfirmCancel {
        pending: Box<Dialog>,
    },
    ConfirmDelete {
        note_id: String,
    },
}

fn new_editor(text: &str) -> TextArea {
    TextArea::new(text)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct AppState {
    doc: Document,
    username: String,
    outline: Outline,
    list_state: ListState,
    focus: Focus,
    body_scroll: u16,
    body_max_scroll: u16,
    body_viewport: u16,
    outline_viewport: u16,
    highlighter: Highlighter,
    dialog: Dialog,
}

impl AppState {
    fn new(doc: Document) -> Self {
        let entry_note_ids: Vec<Vec<String>> = doc
            .entries
            .iter()
            .map(|e| {
                doc.notes_for_line(e.line)
                    .into_iter()
                    .map(|n| n.id.clone())
                    .collect()
            })
            .collect();
        let outline = Outline::new(entry_note_ids);
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            doc,
            username: resolve_username(),
            outline,
            list_state,
            focus: Focus::Outline,
            body_scroll: 0,
            body_max_scroll: 0,
            body_viewport: 0,
            outline_viewport: 0,
            highlighter: Highlighter::new(),
            dialog: Dialog::None,
        }
    }

    fn author(&self) -> String {
        format!("user:{}", self.username)
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Outline => Focus::Body,
            Focus::Body => Focus::Outline,
        };
    }

    fn select_first(&mut self) {
        self.list_state.select(Some(0));
        self.body_scroll = 0;
    }

    fn select_last(&mut self) {
        let len = self.outline.len();
        if len == 0 {
            return;
        }
        self.list_state.select(Some(len - 1));
        self.body_scroll = 0;
    }

    fn select_by(&mut self, delta: isize) {
        let len = self.outline.len();
        if len == 0 {
            return;
        }
        let current = self.list_state.selected().unwrap_or(0) as isize;
        let max = (len - 1) as isize;
        let next = current.saturating_add(delta).clamp(0, max);
        self.list_state.select(Some(next as usize));
        self.body_scroll = 0;
    }

    fn toggle_selected(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        if self.outline.toggle(idx) {
            self.clamp_selection();
            self.body_scroll = 0;
        }
    }

    fn expand_selected(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        if self.outline.expand(idx) {
            self.body_scroll = 0;
        }
    }

    fn collapse_selected(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        match self.outline.collapse(idx) {
            CollapseOutcome::Collapsed => {
                self.clamp_selection();
                self.body_scroll = 0;
            }
            CollapseOutcome::SelectParent(parent) => {
                self.list_state.select(Some(parent));
                self.body_scroll = 0;
            }
            CollapseOutcome::None => {}
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.outline.len();
        if len == 0 {
            self.list_state.select(None);
            return;
        }
        let max = len - 1;
        if let Some(i) = self.list_state.selected()
            && i > max
        {
            self.list_state.select(Some(max));
        }
    }

    fn body_scroll_by(&mut self, delta: i32) {
        let current = i32::from(self.body_scroll);
        let max = i32::from(self.body_max_scroll);
        let next = current.saturating_add(delta).clamp(0, max);
        self.body_scroll = next as u16;
    }

    fn body_scroll_to_top(&mut self) {
        self.body_scroll = 0;
    }

    fn body_scroll_to_bottom(&mut self) {
        self.body_scroll = self.body_max_scroll;
    }

    fn outline_page(&self) -> u16 {
        page_size(self.outline_viewport)
    }

    fn body_page(&self) -> u16 {
        page_size(self.body_viewport)
    }

    /// Entry index for the current selection — the entry row itself, or the
    /// parent entry of the selected note row. None for the session row.
    fn selected_entry_index(&self) -> Option<usize> {
        let row = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))?;
        match &row.kind {
            RowKind::Entry { index } => Some(*index),
            RowKind::Note { entry_index, .. } => Some(*entry_index),
            RowKind::Session => None,
        }
    }

    fn selected_note(&self) -> Option<(usize, &Note)> {
        let row = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))?;
        if let RowKind::Note {
            entry_index,
            note_id,
        } = &row.kind
        {
            self.doc.note(note_id).map(|n| (*entry_index, n))
        } else {
            None
        }
    }

    fn begin_add_note(&mut self) {
        if let Some(entry_index) = self.selected_entry_index() {
            self.dialog = Dialog::AddNote {
                entry_index,
                editor: new_editor(""),
            };
        }
    }

    fn begin_edit_note(&mut self) -> bool {
        let Some((_, note)) = self.selected_note() else {
            return false;
        };
        if note.author != self.author() || !is_comment(&note.name) {
            return false;
        }
        let text = note
            .value
            .0
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| note.value.to_json());
        let note_id = note.id.clone();
        self.dialog = Dialog::EditNote {
            note_id,
            original: text.clone(),
            editor: new_editor(&text),
        };
        true
    }

    fn begin_delete_note(&mut self) {
        let Some((_, note)) = self.selected_note() else {
            return;
        };
        if note.author != self.author() {
            return;
        }
        self.dialog = Dialog::ConfirmDelete {
            note_id: note.id.clone(),
        };
    }
}

fn resolve_username() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

/// Stored note names are namespaced (e.g. `comment.abcd1234`) so multiple
/// notes in the same family can coexist under the DB's (name, target, author)
/// unique key.
fn is_comment(name: &str) -> bool {
    name == "comment" || name.starts_with("comment.")
}

fn handle_note_dialog(state: &mut AppState, key: KeyEvent, db: &Connection) {
    // Newline shortcuts. Terminals without the Kitty keyboard protocol can't
    // distinguish Shift+Enter from Enter, so accept Alt+Enter and Ctrl+J as
    // fallbacks that legacy input layers always report.
    let mods = key.modifiers;
    let is_newline_shortcut = (matches!(key.code, KeyCode::Enter)
        && (mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT)))
        || (matches!(key.code, KeyCode::Char('j')) && mods.contains(KeyModifiers::CONTROL));

    match key.code {
        KeyCode::Enter if !is_newline_shortcut => {
            let empty = match &state.dialog {
                Dialog::AddNote { editor, .. } | Dialog::EditNote { editor, .. } => {
                    editor.text().trim().is_empty()
                }
                _ => true,
            };
            if empty {
                state.dialog = Dialog::None;
            } else {
                commit_note(state, db);
            }
        }
        KeyCode::Esc => {
            let dirty = match &state.dialog {
                Dialog::AddNote { editor, .. } => !editor.text().trim().is_empty(),
                Dialog::EditNote {
                    editor, original, ..
                } => &editor.text() != original,
                _ => false,
            };
            if !dirty {
                state.dialog = Dialog::None;
            } else {
                let pending = std::mem::replace(&mut state.dialog, Dialog::None);
                state.dialog = Dialog::ConfirmCancel {
                    pending: Box::new(pending),
                };
            }
        }
        _ => {
            let editor = match &mut state.dialog {
                Dialog::AddNote { editor, .. } | Dialog::EditNote { editor, .. } => editor,
                _ => return,
            };
            if is_newline_shortcut {
                editor.insert_newline();
            } else {
                editor.input(key);
            }
        }
    }
}

fn commit_note(state: &mut AppState, db: &Connection) {
    match std::mem::replace(&mut state.dialog, Dialog::None) {
        Dialog::AddNote {
            entry_index,
            editor,
        } => {
            let Some(entry) = state.doc.entries.get(entry_index) else {
                return;
            };
            let text = editor.text();
            let target = NoteTarget::Session(
                SessionTarget::new(&state.doc.session.id).with_line(entry.line),
            );
            // Notes are keyed by (name, target, author) in the DB, so a literal
            // "comment" name caps users at one comment per line. Suffix with a
            // short slice of the note's own UUID to keep the key unique while
            // the display layer renders the family name "comment".
            let mut note = Note::new(target, "comment", NoteValue::from(text), &state.author());
            let suffix: String = note.id.chars().take(8).collect();
            note.name = format!("comment.{suffix}");
            if let Ok(()) = note::insert(db, &note) {
                let id = note.id.clone();
                state.doc.add_note(note);
                if let Some(row) = state.outline.add_note(entry_index, id) {
                    state.list_state.select(Some(row));
                    state.body_scroll = 0;
                }
            }
        }
        Dialog::EditNote {
            note_id, editor, ..
        } => {
            let Some(existing) = state.doc.note(&note_id).cloned() else {
                return;
            };
            let mut updated = existing;
            updated.value = NoteValue::from(editor.text());
            if let Ok(new) = note::replace(db, &note_id, &updated) {
                state.doc.replace_note_value(
                    &note_id,
                    new.value,
                    new.modified.unwrap_or_else(now_ms),
                );
            }
        }
        other => state.dialog = other,
    }
}

fn handle_confirm_cancel(state: &mut AppState, code: KeyCode) {
    let pending = match std::mem::replace(&mut state.dialog, Dialog::None) {
        Dialog::ConfirmCancel { pending } => pending,
        other => {
            state.dialog = other;
            return;
        }
    };
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            // Discard the editor; dialog stays None.
        }
        _ => state.dialog = *pending,
    }
}

fn handle_confirm_delete(state: &mut AppState, code: KeyCode, db: &Connection) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Dialog::ConfirmDelete { note_id } =
                std::mem::replace(&mut state.dialog, Dialog::None)
                && note::delete(db, &note_id).is_ok()
            {
                state.doc.remove_note(&note_id);
                if let Some(entry_index) = state.outline.remove_note(&note_id) {
                    let target_row = state.outline.rows().iter().position(
                        |r| matches!(&r.kind, RowKind::Entry { index } if *index == entry_index),
                    );
                    if let Some(r) = target_row {
                        state.list_state.select(Some(r));
                    } else {
                        state.clamp_selection();
                    }
                    state.body_scroll = 0;
                }
            }
        }
        _ => state.dialog = Dialog::None,
    }
}

fn page_size(viewport: u16) -> u16 {
    let v = u32::from(viewport);
    ((v * 9) / 10).max(1) as u16
}

fn draw(frame: &mut Frame, state: &mut AppState, turns: Option<&[Option<usize>]>) {
    let [header_area, middle_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [outline_area, body_area] =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).areas(middle_area);

    let header = Paragraph::new(Line::from(format!("Session {}", state.doc.session.id)))
        .style(style::header());
    frame.render_widget(header, header_area);

    draw_outline(frame, state, outline_area, turns);
    draw_body(frame, state, body_area);

    let footer = Paragraph::new(Line::from(footer_hint(state))).style(style::footer());
    frame.render_widget(footer, footer_area);

    draw_dialog(frame, &mut state.dialog);
}

fn footer_hint(state: &AppState) -> String {
    let mut hints = vec![
        "q quit",
        "Tab pane",
        "j/k g/G PgUp/PgDn",
        "Enter ◂ ▸",
        "n note",
    ];
    if let Some((_, note)) = state.selected_note()
        && note.author == state.author()
    {
        if is_comment(&note.name) {
            hints.push("e edit");
        }
        hints.push("d delete");
    }
    hints.join(" · ")
}

fn draw_outline(
    frame: &mut Frame,
    state: &mut AppState,
    area: Rect,
    turns: Option<&[Option<usize>]>,
) {
    let active = state.focus == Focus::Outline;

    state.outline_viewport = area.height.saturating_sub(2);

    let selected = state.list_state.selected();
    let items: Vec<ListItem> = state
        .outline
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| row_to_item(row, &state.doc, Some(i) == selected, turns))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(style::panel_border(active))
                .title("Entries"),
        )
        .highlight_style(style::selection());
    frame.render_stateful_widget(list, area, &mut state.list_state);

    let max_offset = state
        .outline
        .len()
        .saturating_sub(state.outline_viewport as usize);
    let offset = state.list_state.offset();
    let mut scrollbar_state = ScrollbarState::new(max_offset).position(offset);
    frame.render_stateful_widget(
        scrollbar(active),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar_state,
    );
}

fn row_to_item(
    row: &crate::outline::Row,
    doc: &Document,
    is_selected: bool,
    turns: Option<&[Option<usize>]>,
) -> ListItem<'static> {
    let indent = "  ".repeat(row.level.saturating_sub(1));
    let glyph = if row.has_children {
        if row.expanded { "▼ " } else { "▶ " }
    } else {
        "  "
    };
    let prefix = Span::raw(format!("{indent}{glyph}"));
    let line = match &row.kind {
        RowKind::Session => Line::from(vec![prefix, Span::raw(doc.session.id.clone())]),
        RowKind::Entry { index } => {
            let kind = doc
                .entries
                .get(*index)
                .map_or("?".to_string(), |e| e.label().to_string());
            let number_style = if is_selected {
                Style::new()
            } else {
                style::text_dim()
            };
            let turn_suffix = turns
                .and_then(|t| t.get(*index).copied().flatten())
                .map(|n| format!(" {}", circled_number(n)))
                .unwrap_or_default();
            Line::from(vec![
                prefix,
                Span::styled(format!("{} ", index + 1), number_style),
                Span::raw(format!("{kind}{turn_suffix}")),
            ])
        }
        RowKind::Note { note_id, .. } => {
            let label = doc
                .note(note_id)
                .map(|n| n.name.clone())
                .unwrap_or_else(|| "?".to_string());
            Line::from(vec![prefix, Span::raw(label)])
        }
    };
    ListItem::new(line)
}

fn draw_body(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let active = state.focus == Focus::Body;
    let row_kind = state
        .list_state
        .selected()
        .and_then(|i| state.outline.row(i))
        .map(|r| r.kind.clone());

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(style::panel_border(active))
        .title(body_title(&state.doc, row_kind.as_ref()));
    let inner = outer.inner(area);
    state.body_viewport = inner.height;
    frame.render_widget(outer, area);

    match row_kind {
        Some(RowKind::Session) => {
            let yaml = state.doc.session.yaml();
            let lines = state.highlighter.highlight(&yaml, "yaml");
            draw_scrollable(frame, state, lines, inner);
        }
        Some(RowKind::Entry { index }) => {
            let snap = state.doc.entries.get(index).map(|e| EntrySnap {
                yaml: e.yaml(),
                message: e.message().cloned(),
            });
            match snap {
                Some(snap) => draw_entry(frame, state, &snap, inner),
                None => draw_placeholder(frame, "(missing entry)", inner),
            }
        }
        Some(RowKind::Note { note_id, .. }) => match state.doc.note(&note_id) {
            Some(note) => draw_note(frame, note, inner),
            None => draw_placeholder(frame, "(missing note)", inner),
        },
        None => draw_placeholder(frame, "(no selection)", inner),
    }

    draw_body_scrollbar(frame, state, active, area);
}

fn body_title(doc: &Document, kind: Option<&RowKind>) -> String {
    match kind {
        Some(RowKind::Session) => doc.session.id.clone(),
        Some(RowKind::Entry { index }) => entry_header(doc, *index),
        Some(RowKind::Note {
            entry_index,
            note_id,
        }) => {
            let head = entry_header(doc, *entry_index);
            let name = doc
                .note(note_id)
                .map(|n| n.name.clone())
                .unwrap_or_else(|| "?".to_string());
            format!("{head} • {name}")
        }
        None => "Body".to_string(),
    }
}

fn entry_header(doc: &Document, index: usize) -> String {
    let kind = doc
        .entries
        .get(index)
        .map_or("?".to_string(), |e| e.label().to_string());
    format!("{} {}", index + 1, kind)
}

fn draw_note(frame: &mut Frame, note: &Note, area: Rect) {
    let value = match &note.value.0 {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let modified_suffix = note
        .modified
        .map(|m| format!(" · edited {}", format_ms(m)))
        .unwrap_or_default();
    let header = Line::from(Span::styled(
        format!(
            "{} · {}{}",
            note.author,
            format_ms(note.created),
            modified_suffix
        ),
        style::text_dim(),
    ));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(header);
    lines.push(Line::from(""));
    for l in value.lines() {
        lines.push(Line::from(l.to_string()));
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().padding(Padding::uniform(1)));
    frame.render_widget(p, area);
}

fn format_ms(ms: i64) -> String {
    // Render as a UTC ISO-ish stamp using gage-core's helper if available;
    // otherwise fall back to the raw seconds value. Keep it simple: the
    // intent here is human orientation, not parseable output.
    let secs = ms / 1000;
    let datetime = chrono_like(secs);
    datetime.unwrap_or_else(|| format!("{secs}s"))
}

fn chrono_like(secs: i64) -> Option<String> {
    // Lightweight UTC formatter: yyyy-mm-dd hh:mm without pulling in chrono.
    // Days from epoch -> civil date via Howard Hinnant's algorithm.
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days)?;
    let h = time / 3600;
    let mi = (time % 3600) / 60;
    Some(format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}"))
}

fn civil_from_days(z: i64) -> Option<(i32, u32, u32)> {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    Some((y, m, d))
}

struct EntrySnap {
    yaml: String,
    message: Option<serde_json::Value>,
}

fn draw_entry(frame: &mut Frame, state: &mut AppState, entry: &EntrySnap, area: Rect) {
    let mut sections: Vec<Section> = Vec::new();
    if let Some(message) = &entry.message {
        let panel = Paragraph::new(message::render(message))
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::uniform(1)));
        sections.push(Section::from_paragraph(panel, area.width));
        sections.push(Section::from_paragraph(
            Paragraph::new(Line::from(Span::styled("--- raw ---", style::text_dim()))),
            area.width,
        ));
    }
    sections.push(Section::from_paragraph(
        Paragraph::new(state.highlighter.highlight(&entry.yaml, "yaml")).wrap(Wrap { trim: false }),
        area.width,
    ));

    draw_stack(frame, state, sections, area);
}

type RenderFn = Box<dyn FnOnce(Rect, &mut ratatui::buffer::Buffer)>;

struct Section {
    widget: RenderFn,
    height: u16,
}

impl Section {
    fn from_paragraph(p: Paragraph<'static>, width: u16) -> Self {
        let height = u16::try_from(p.line_count(width)).unwrap_or(u16::MAX);
        Self {
            widget: Box::new(move |area, buf| {
                ratatui::widgets::Widget::render(p, area, buf);
            }),
            height,
        }
    }
}

fn draw_stack(frame: &mut Frame, state: &mut AppState, sections: Vec<Section>, area: Rect) {
    let total: u16 = sections
        .iter()
        .map(|s| s.height)
        .fold(0u16, |a, b| a.saturating_add(b));
    state.body_max_scroll = total.saturating_sub(area.height);
    if state.body_scroll > state.body_max_scroll {
        state.body_scroll = state.body_max_scroll;
    }
    if total == 0 || area.width == 0 || area.height == 0 {
        return;
    }

    let virt_rect = Rect {
        x: 0,
        y: 0,
        width: area.width,
        height: total,
    };
    let mut virt = ratatui::buffer::Buffer::empty(virt_rect);
    let mut y: u16 = 0;
    for section in sections {
        let section_rect = Rect {
            x: 0,
            y,
            width: area.width,
            height: section.height,
        };
        (section.widget)(section_rect, &mut virt);
        y = y.saturating_add(section.height);
    }

    let dst = frame.buffer_mut();
    let scroll = state.body_scroll;
    for row in 0..area.height {
        let src_y = scroll.saturating_add(row);
        if src_y >= total {
            break;
        }
        for col in 0..area.width {
            if let (Some(src_cell), Some(dst_cell)) = (
                virt.cell(ratatui::layout::Position::new(col, src_y)),
                dst.cell_mut(ratatui::layout::Position::new(area.x + col, area.y + row)),
            ) {
                *dst_cell = src_cell.clone();
            }
        }
    }
}

fn draw_scrollable(frame: &mut Frame, state: &mut AppState, lines: Vec<Line<'static>>, area: Rect) {
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(area.width)).unwrap_or(u16::MAX);
    state.body_max_scroll = total.saturating_sub(area.height);
    if state.body_scroll > state.body_max_scroll {
        state.body_scroll = state.body_max_scroll;
    }
    frame.render_widget(paragraph.scroll((state.body_scroll, 0)), area);
}

fn draw_placeholder(frame: &mut Frame, text: &'static str, area: Rect) {
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_body_scrollbar(frame: &mut Frame, state: &AppState, active: bool, area: Rect) {
    let mut scrollbar_state =
        ScrollbarState::new(state.body_max_scroll as usize).position(state.body_scroll as usize);
    frame.render_stateful_widget(
        scrollbar(active),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar_state,
    );
}

fn draw_dialog(frame: &mut Frame, dialog: &mut Dialog) {
    match dialog {
        Dialog::None => {}
        Dialog::AddNote { editor, .. } => draw_editor(frame, "Add note", editor),
        Dialog::EditNote { editor, .. } => draw_editor(frame, "Edit note", editor),
        Dialog::ConfirmCancel { pending } => {
            match pending.as_mut() {
                Dialog::AddNote { editor, .. } => draw_editor(frame, "Add note", editor),
                Dialog::EditNote { editor, .. } => draw_editor(frame, "Edit note", editor),
                _ => {}
            }
            draw_confirm(frame, "Cancel your changes?");
        }
        Dialog::ConfirmDelete { .. } => {
            draw_confirm(frame, "Delete this note? This cannot be undone.")
        }
    }
}

const EDITOR_BODY_HEIGHT: u16 = 8;

fn draw_editor(frame: &mut Frame, title: &str, editor: &mut TextArea) {
    let area = editor_rect(frame.area());
    let (body_area, hint_area) = draw_dialog_block(frame, area, title);

    // Compute wrap against the body width minus a potential 1-col scrollbar.
    // We don't yet know if the bar is needed, but it only appears once content
    // exceeds the viewport, so probe at the narrower width first.
    let probe_width = body_area.width.saturating_sub(1).max(1);
    let total_visual = editor.visual_row_count(probe_width);
    let needs_bar = total_visual > body_area.height as usize;
    let (text_area, bar_area) = if needs_bar {
        let [t, b] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(body_area);
        (t, Some(b))
    } else {
        (body_area, None)
    };
    let cursor_pos = editor.render(text_area, frame.buffer_mut(), Style::default());
    if let Some((x, y)) = cursor_pos {
        frame.set_cursor_position((x, y));
    }

    if let Some(bar_area) = bar_area {
        let cursor_row = editor.visual_cursor_row(text_area.width);
        let mut sb_state = ScrollbarState::new(editor.visual_row_count(text_area.width))
            .viewport_content_length(text_area.height as usize)
            .position(cursor_row);
        frame.render_stateful_widget(scrollbar(true), bar_area, &mut sb_state);
    }

    draw_hint(
        frame,
        hint_area,
        "Enter save · Shift/Alt+Enter or Ctrl+J newline · Esc cancel",
    );
}

/// Editor dialog sizing: width capped at 80 columns, height fits a fixed
/// `EDITOR_BODY_HEIGHT` body plus the 5 chrome rows (borders, top pad, gap,
/// hint). The body scrolls so content can exceed its visible height.
fn editor_rect(frame: Rect) -> Rect {
    let width = frame.width.saturating_sub(4).clamp(30, 80);
    let height = (EDITOR_BODY_HEIGHT + 5).min(frame.height.saturating_sub(2));
    let x = frame.x + (frame.width.saturating_sub(width)) / 2;
    let y = frame.y + (frame.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn draw_confirm(frame: &mut Frame, message: &str) {
    let area = confirm_rect(frame.area(), message);
    let (body_area, hint_area) = draw_dialog_block(frame, area, "Confirm");

    frame.render_widget(
        Paragraph::new(message.to_string())
            .wrap(Wrap { trim: false })
            .alignment(ratatui::layout::Alignment::Center),
        body_area,
    );
    draw_hint(frame, hint_area, "y / n");
}

/// Confirm prompts size to their content: width capped at 60 columns and at
/// most half the frame; height matches the wrapped message plus the dialog
/// chrome — top + bottom border, 1-row top pad, 1-row gap, and 1-row hint
/// (5 chrome rows total).
fn confirm_rect(frame: Rect, message: &str) -> Rect {
    let max_w = frame.width.saturating_sub(4).clamp(20, 60);
    let inner_w = max_w.saturating_sub(4);
    let lines = wrapped_line_count(message, inner_w);
    let height = (lines + 5).min(frame.height.saturating_sub(2));
    let width = max_w;
    let x = frame.x + (frame.width.saturating_sub(width)) / 2;
    let y = frame.y + (frame.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let p = Paragraph::new(text.to_string()).wrap(Wrap { trim: false });
    u16::try_from(p.line_count(width)).unwrap_or(u16::MAX)
}

/// Renders the bordered dialog frame and returns `(body_area, hint_area)`.
/// Layout inside the border:
///
/// ```text
/// -----
/// | <space>
/// | body
/// | <space>
/// | hint
/// -----
/// ```
///
/// The hint sits flush with the bottom border; padding around the body is one
/// row on top and one column on each side.
fn draw_dialog_block(frame: &mut Frame, area: Rect, title: &str) -> (Rect, Rect) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title.to_string(), style::text_dim()))
        .border_style(style::text_dim())
        .padding(Padding::new(1, 1, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [body_area, _gap, hint_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    (body_area, hint_area)
}

fn draw_hint(frame: &mut Frame, area: Rect, hint: &str) {
    let p = Paragraph::new(Line::from(Span::styled(
        hint.to_string(),
        style::text_dim(),
    )))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(p, area);
}

fn compute_turns(doc: &Document) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(doc.entries.len());
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut next: usize = 0;
    for entry in &doc.entries {
        let turn = if entry.entry_type() == "assistant" {
            entry
                .message()
                .and_then(|m| m.get("id"))
                .and_then(|v| v.as_str())
                .map(|id| match seen.get(id) {
                    Some(&n) => n,
                    None => {
                        next += 1;
                        seen.insert(id.to_string(), next);
                        next
                    }
                })
        } else {
            None
        };
        out.push(turn);
    }
    out
}

fn circled_number(n: usize) -> String {
    if (1..=20).contains(&n) {
        return char::from_u32(0x2460 + (n as u32) - 1)
            .expect("U+2460..=U+2473 are valid scalars")
            .to_string();
    }
    n.to_string().chars().map(circled_digit).collect()
}

fn circled_digit(d: char) -> char {
    match d {
        '0' => '\u{24EA}',
        '1'..='9' => char::from_u32(0x2460 + (d as u32 - '1' as u32))
            .expect("U+2460..=U+2468 are valid scalars"),
        _ => d,
    }
}

fn scrollbar(active: bool) -> Scrollbar<'static> {
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("┃")
        .track_symbol(Some("│"))
        .style(style::scrollbar(active))
}
