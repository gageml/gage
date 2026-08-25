//! TUI shell — header / outline / body / footer. Tab toggles which pane is
//! active. In the outline: j/k/g/G/PgUp/PgDn navigate, Enter or Space
//! toggles expansion (Enter on a user comment note edits it instead). In
//! the body: j/k/g/G/PgUp/PgDn scroll. `n` opens a note dialog over the
//! selected entry; `e` edits the selected user comment note; `d` deletes
//! the selected user-authored note. →/← expand and collapse (← moves to
//! the parent at a terminus). `[`/`]` are deliberately unbound so a host
//! embedding the view can step between sessions with them.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use gage_db::note::{self, Note, NoteValue};
use gage_db::rusqlite::Connection;
use gage_db::target::{NoteTarget, SessionTarget};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use serde_json::Value;

use crate::doc::Document;
use crate::hint;
use crate::options::ViewOptions;
use crate::outline::{CollapseOutcome, Outline, RowKind};
use crate::picker::{self, PickItem, Picker, PickerAction};
use crate::session;
use crate::syntax::Highlighter;
use crate::textarea::TextArea;
use crate::{message, styles};

pub fn run(
    terminal: &mut DefaultTerminal,
    doc: Option<Document>,
    options: &ViewOptions,
    db: &Connection,
) -> io::Result<()> {
    let doc = match doc {
        Some(doc) => doc,
        // No session given: the open dialog doubles as the initial
        // picker; canceling it exits the app.
        None => match standalone_pick(terminal, db)? {
            Some(doc) => doc,
            None => return Ok(()),
        },
    };
    let mut state = AppState::new(doc, options, DocSource::Query);
    loop {
        terminal.draw(|frame| draw(frame, &mut state))?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && handle_key(&mut state, key, db)? == KeyOutcome::Close
        {
            return Ok(());
        }
    }
}

/// Run the open dialog on a blank background until the user picks a
/// session or cancels.
fn standalone_pick(
    terminal: &mut DefaultTerminal,
    db: &Connection,
) -> io::Result<Option<Document>> {
    let mut picker = session_picker(None)?;
    loop {
        terminal.draw(|frame| {
            draw_empty_shell(frame);
            picker.draw(frame);
        })?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match picker.handle_key(key.code) {
                PickerAction::None => {}
                PickerAction::Close => return Ok(None),
                PickerAction::Open(id) => return load_doc_blocking(&id, db).map(Some),
            }
        }
    }
}

/// The app frame with no session open: empty panels behind the
/// initial picker, so picking renders in place with no flash.
fn draw_empty_shell(frame: &mut Frame) {
    let [header_area, middle_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(
        Paragraph::new(Line::from("")).style(styles::Panel::header()),
        header_area,
    );
    let [outline_area, body_area] =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).areas(middle_area);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(styles::Panel::border(false))
            .title("Entries"),
        outline_area,
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(styles::Panel::border(false)),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from("")).style(styles::Panel::footer()),
        footer_area,
    );
}

/// Build the session-open picker from the corpus's recent sessions.
fn session_picker(current: Option<&str>) -> io::Result<Picker> {
    let items = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(session::list_recent(200))
    })
    .map_err(|e| io::Error::other(e.to_string()))?;
    let items = items
        .into_iter()
        .map(|item| {
            let short = gage_core::uuid::short_uuid(&item.id).to_string();
            PickItem {
                line: Line::from(vec![
                    Span::styled(short, styles::Text::id()),
                    Span::styled(
                        format!("  {:>4}  ", picker::ago(item.mtime_ms)),
                        styles::Text::dim(),
                    ),
                    Span::raw(item.title),
                ]),
                id: item.id,
            }
        })
        .collect();
    Ok(Picker::new("Open session", items, current))
}

/// Load a session document from the sync event loop.
fn load_doc_blocking(session_id: &str, db: &Connection) -> io::Result<Document> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(session::load(session_id, db))
    })
    .map_err(|e| io::Error::other(e.to_string()))
}

/// What a key press did, for hosts that embed the view. `Close` is a
/// request to dismiss the view (`q`/`Esc`); `Ignored` means the key had
/// no effect at the current position, letting a host bind its own
/// meaning (e.g. `[`/`]` stepping to another session).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyOutcome {
    Consumed,
    Close,
    Ignored,
}

/// Apply one key press to the view. Dialog input takes precedence over
/// global keys.
pub(crate) fn handle_key(
    state: &mut AppState,
    key: KeyEvent,
    db: &Connection,
) -> io::Result<KeyOutcome> {
    match &mut state.dialog {
        Dialog::AddNote { .. } | Dialog::EditNote { .. } => {
            handle_note_dialog(state, key, db);
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::ConfirmCancel { .. } => {
            handle_confirm_cancel(state, key.code);
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::ConfirmDelete { .. } => {
            handle_confirm_delete(state, key.code, db);
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::OpenSession(picker) => {
            match picker.handle_key(key.code) {
                PickerAction::None => {}
                PickerAction::Close => state.dialog = Dialog::None,
                PickerAction::Open(id) => {
                    state.dialog = Dialog::None;
                    if id != state.doc.session.id {
                        let doc = load_doc_blocking(&id, db)?;
                        state.replace_doc(doc);
                    }
                }
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::Options => {
            match key.code {
                KeyCode::Char('d') => {
                    state.dialog = Dialog::None;
                    state.toggle_detail();
                }
                KeyCode::Char('t') => {
                    state.dialog = Dialog::None;
                    state.toggle_turns();
                }
                KeyCode::Char('q') | KeyCode::Char('v') | KeyCode::Esc => {
                    state.dialog = Dialog::None;
                }
                _ => {}
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::None => {}
    }
    let outcome = match key.code {
        KeyCode::Char('q') | KeyCode::Esc => KeyOutcome::Close,
        KeyCode::Tab | KeyCode::BackTab => {
            state.toggle_focus();
            KeyOutcome::Consumed
        }
        KeyCode::Down | KeyCode::Char('j') => {
            match state.focus {
                Focus::Outline => state.select_by(1),
                Focus::Body => state.body_scroll_by(1),
            }
            KeyOutcome::Consumed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            match state.focus {
                Focus::Outline => state.select_by(-1),
                Focus::Body => state.body_scroll_by(-1),
            }
            KeyOutcome::Consumed
        }
        KeyCode::Char('g') => {
            match state.focus {
                Focus::Outline => state.select_first(),
                Focus::Body => state.body_scroll_to_top(),
            }
            KeyOutcome::Consumed
        }
        KeyCode::Char('G') => {
            match state.focus {
                Focus::Outline => state.select_last(),
                Focus::Body => state.body_scroll_to_bottom(),
            }
            KeyOutcome::Consumed
        }
        KeyCode::PageDown => {
            match state.focus {
                Focus::Outline => state.select_by(state.outline_page() as isize),
                Focus::Body => state.body_scroll_by(state.body_page() as i32),
            }
            KeyOutcome::Consumed
        }
        KeyCode::PageUp => {
            match state.focus {
                Focus::Outline => state.select_by(-(state.outline_page() as isize)),
                Focus::Body => state.body_scroll_by(-(state.body_page() as i32)),
            }
            KeyOutcome::Consumed
        }
        KeyCode::Enter if state.focus == Focus::Outline => {
            if !state.begin_edit_note() {
                state.toggle_selected();
            }
            KeyOutcome::Consumed
        }
        KeyCode::Char(' ') if state.focus == Focus::Outline => {
            state.toggle_selected();
            KeyOutcome::Consumed
        }
        KeyCode::Right if state.focus == Focus::Outline => {
            state.expand_selected();
            KeyOutcome::Consumed
        }
        KeyCode::Left if state.focus == Focus::Outline => {
            state.collapse_selected();
            KeyOutcome::Consumed
        }
        KeyCode::Char('l')
            if key.modifiers.contains(KeyModifiers::CONTROL) && state.focus == Focus::Outline =>
        {
            state.center_selected();
            KeyOutcome::Consumed
        }
        KeyCode::Char('n') => {
            state.begin_add_note();
            KeyOutcome::Consumed
        }
        KeyCode::Char('v') => {
            state.dialog = Dialog::Options;
            KeyOutcome::Consumed
        }
        KeyCode::Char('o') => {
            state.dialog = Dialog::OpenSession(session_picker(Some(&state.doc.session.id))?);
            KeyOutcome::Consumed
        }
        KeyCode::Char('e') => {
            state.begin_edit_note();
            KeyOutcome::Consumed
        }
        KeyCode::Char('d') => {
            state.begin_delete_note();
            KeyOutcome::Consumed
        }
        KeyCode::Char('r') => {
            state.reload(db)?;
            KeyOutcome::Consumed
        }
        _ => KeyOutcome::Ignored,
    };
    Ok(outcome)
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Outline,
    Body,
}

/// Where the document came from, so `reload` can re-read it.
pub(crate) enum DocSource {
    /// The indexed corpus via `gage-query` (`session::load`)
    Query,
    /// A session JSONL read directly (`session::load_from_path`) — used
    /// for sessions outside the active corpus index
    Path(std::path::PathBuf),
}

/// Snapshot of the view's UI position, for hosts that re-open sessions
/// and restore where the user left off.
#[derive(Clone, Copy)]
pub(crate) struct SavedUi {
    selected: Option<usize>,
    offset: usize,
    body_scroll: u16,
    focus_body: bool,
}

enum Dialog {
    None,
    AddNote {
        /// Target entry; None for a session-level note (the
        /// `<Session>` row or one of its notes selected)
        entry_index: Option<usize>,
        editor: TextArea,
        /// Dialog caption, fixed at open per the note kind
        title: &'static str,
    },
    EditNote {
        note_id: String,
        original: String,
        editor: TextArea,
        /// Dialog caption, fixed at open per the note kind
        title: &'static str,
    },
    /// Sub-dialog layered over the editor when the user pressed Esc with
    /// non-empty content. `y` discards `pending`; anything else restores it.
    ConfirmCancel {
        pending: Box<Dialog>,
    },
    ConfirmDelete {
        note_id: String,
    },
    /// Session-open picker (`o`)
    OpenSession(Picker),
    /// View-options toggles (`v`)
    Options,
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

pub(crate) struct AppState {
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
    turns: Option<Vec<Option<usize>>>,
    /// Annotation mode: `n` writes `open` notes (one per node)
    /// instead of `comment.{rand}`
    annotate: bool,
    source: DocSource,
}

impl AppState {
    pub(crate) fn new(doc: Document, options: &ViewOptions, source: DocSource) -> Self {
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let outline = Outline::new(
            session_note_ids,
            entry_note_ids,
            entry_hidden(&doc),
            options.show_detail,
        );
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let turns = options.show_turns.then(|| compute_turns(&doc));
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
            turns,
            annotate: options.annotate,
            source,
        }
    }

    pub(crate) fn save_ui(&self) -> SavedUi {
        SavedUi {
            selected: self.list_state.selected(),
            offset: self.list_state.offset(),
            body_scroll: self.body_scroll,
            focus_body: self.focus == Focus::Body,
        }
    }

    pub(crate) fn restore_ui(&mut self, saved: &SavedUi) {
        let max = self.outline.len().saturating_sub(1);
        self.list_state
            .select(saved.selected.map(|i| i.min(max)).or(Some(0)));
        *self.list_state.offset_mut() = saved.offset;
        self.body_scroll = saved.body_scroll;
        self.focus = if saved.focus_body {
            Focus::Body
        } else {
            Focus::Outline
        };
    }

    /// Select entry `index`'s outline row and scroll it into view, for
    /// hosts opening the view at a specific entry.
    pub(crate) fn select_entry(&mut self, index: usize) {
        let Some(row) = self
            .outline
            .rows()
            .iter()
            .position(|r| matches!(r.kind, RowKind::Entry { index: i } if i == index))
        else {
            return;
        };
        self.list_state.select(Some(row));
        self.body_scroll = 0;
        self.center_selected();
    }

    fn author(&self) -> String {
        format!("user:{}", self.username)
    }

    /// Count of the viewer's own notes on the open session, for hosts
    /// tracking annotation coverage.
    pub(crate) fn own_note_count(&self) -> usize {
        let author = self.author();
        self.doc.notes.iter().filter(|n| n.author == author).count()
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

    /// Collapse the selected row, or move to its parent when there is
    /// nothing to collapse (the session-view convention).
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

    fn reload(&mut self, db: &Connection) -> io::Result<()> {
        let session_id = self.doc.session.id.clone();
        let prior = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))
            .map(|r| r.kind.clone());
        let doc = match &self.source {
            DocSource::Query => tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(session::load(&session_id, db))
            }),
            DocSource::Path(path) => session::load_from_path(&session_id, path, db),
        }
        .map_err(|e| io::Error::other(e.to_string()))?;
        if self.turns.is_some() {
            self.turns = Some(compute_turns(&doc));
        }
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let hidden = entry_hidden(&doc);
        self.doc = doc;
        self.outline
            .reload(session_note_ids, entry_note_ids, hidden);
        let new_sel = prior
            .and_then(|kind| {
                self.outline
                    .rows()
                    .iter()
                    .position(|r| same_row(&r.kind, &kind))
            })
            .or(Some(0));
        self.list_state.select(new_sel);
        Ok(())
    }

    /// Toggle the entry filter, keeping the selection when its row
    /// survives the rebuild.
    fn toggle_detail(&mut self) {
        let prior = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))
            .map(|r| r.kind.clone());
        let detail = !self.outline.detail();
        self.outline.set_detail(detail);
        // Reselect the same row; a filtered-out entry falls back to
        // the nearest preceding visible entry
        let new_sel = prior
            .and_then(|kind| {
                self.outline
                    .rows()
                    .iter()
                    .position(|r| same_row(&r.kind, &kind))
                    .or_else(|| match kind {
                        RowKind::Entry { index } => nearest_prev_entry(&self.outline, index),
                        _ => None,
                    })
            })
            .or(Some(0));
        self.list_state.select(new_sel);
    }

    fn toggle_turns(&mut self) {
        self.turns = match self.turns.take() {
            Some(_) => None,
            None => Some(compute_turns(&self.doc)),
        };
    }

    /// Swap in a different session's document, resetting the UI to the
    /// top of the new session.
    fn replace_doc(&mut self, doc: Document) {
        if self.turns.is_some() {
            self.turns = Some(compute_turns(&doc));
        }
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let hidden = entry_hidden(&doc);
        self.doc = doc;
        self.outline
            .reload(session_note_ids, entry_note_ids, hidden);
        self.list_state.select(Some(0));
        *self.list_state.offset_mut() = 0;
        self.body_scroll = 0;
        self.focus = Focus::Outline;
        self.source = DocSource::Query;
    }

    fn center_selected(&mut self) {
        let Some(sel) = self.list_state.selected() else {
            return;
        };
        let half = (self.outline_viewport / 2) as usize;
        *self.list_state.offset_mut() = sel.saturating_sub(half);
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
            RowKind::Note { entry_index, .. } => *entry_index,
            RowKind::Session => None,
        }
    }

    fn selected_note(&self) -> Option<&Note> {
        let row = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))?;
        if let RowKind::Note { note_id, .. } = &row.kind {
            self.doc.note(note_id)
        } else {
            None
        }
    }

    fn begin_add_note(&mut self) {
        if self.list_state.selected().is_none() {
            return;
        }
        let entry_index = self.selected_entry_index();
        // Annotation mode: one open code per node — a second `n` on an
        // already-coded node edits the existing note
        if self.annotate {
            if let Some(note) = self.own_open_code(entry_index) {
                let (note_id, text) = (note.id.clone(), note_text(note));
                self.open_note_editor(note_id, text, "Edit open code");
                return;
            }
            self.dialog = Dialog::AddNote {
                entry_index,
                editor: new_editor(""),
                title: "Add open code",
            };
            return;
        }
        self.dialog = Dialog::AddNote {
            entry_index,
            editor: new_editor(""),
            title: "Add comment",
        };
    }

    /// The viewer's `open` note on the given node, if any.
    fn own_open_code(&self, entry_index: Option<usize>) -> Option<&Note> {
        let author = self.author();
        let notes = match entry_index {
            Some(i) => self.doc.notes_for_line(self.doc.entries.get(i)?.line),
            None => self.doc.session_notes(),
        };
        notes
            .into_iter()
            .find(|n| n.author == author && n.name == "open")
    }

    fn begin_edit_note(&mut self) -> bool {
        let Some(note) = self.selected_note() else {
            return false;
        };
        if note.author != self.author() || !is_editable(&note.name) {
            return false;
        }
        let title = if is_comment(&note.name) {
            "Edit comment"
        } else {
            "Edit open code"
        };
        let (note_id, text) = (note.id.clone(), note_text(note));
        self.open_note_editor(note_id, text, title);
        true
    }

    fn open_note_editor(&mut self, note_id: String, text: String, title: &'static str) {
        self.dialog = Dialog::EditNote {
            note_id,
            original: text.clone(),
            editor: new_editor(&text),
            title,
        };
    }

    fn begin_delete_note(&mut self) {
        let Some(note) = self.selected_note() else {
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

/// Visible-row position of the entry nearest to (at or before)
/// `target`, for reselecting after a filter hides the selected entry.
/// Entry rows appear in ascending index order, so the last qualifying
/// row is the nearest.
fn nearest_prev_entry(outline: &Outline, target: usize) -> Option<usize> {
    outline
        .rows()
        .iter()
        .enumerate()
        .rev()
        .find(|(_, r)| matches!(r.kind, RowKind::Entry { index } if index <= target))
        .map(|(i, _)| i)
}

/// Row-identity comparison for reselecting after an outline rebuild.
fn same_row(a: &RowKind, b: &RowKind) -> bool {
    match (a, b) {
        (RowKind::Session, RowKind::Session) => true,
        (RowKind::Entry { index: a }, RowKind::Entry { index: b }) => a == b,
        (RowKind::Note { note_id: a, .. }, RowKind::Note { note_id: b, .. }) => a == b,
        _ => false,
    }
}

/// Low-signal entries hidden by the default view filter: user meta
/// entries, attachments, and session bookkeeping types. Everything
/// else — user and assistant text, thinking, tool use, tool results (a
/// successful result can contain the actual error) — shows.
fn entry_hidden(doc: &Document) -> Vec<bool> {
    const HIDDEN_TYPES: &[&str] = &[
        "attachment",
        "queue-operation",
        "file-history-snapshot",
        "ai-title",
        "last-prompt",
    ];
    doc.entries
        .iter()
        .map(|e| e.label() == "meta" || HIDDEN_TYPES.contains(&e.entry_type()))
        .collect()
}

/// Outline projection of the document's notes — session-level note ids, then
/// per-entry note ids keyed by entry position.
fn note_projection(doc: &Document) -> (Vec<String>, Vec<Vec<String>>) {
    let session = doc
        .session_notes()
        .into_iter()
        .map(|n| n.id.clone())
        .collect();
    let entries = doc
        .entries
        .iter()
        .map(|e| {
            doc.notes_for_line(e.line)
                .into_iter()
                .map(|n| n.id.clone())
                .collect()
        })
        .collect();
    (session, entries)
}

/// A note's text for the editor: the plain string when the value is
/// one, otherwise its JSON form.
fn note_text(note: &Note) -> String {
    note.value
        .0
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| note.value.to_json())
}

/// User-editable note names: comments and the viewer's open codes.
fn is_editable(name: &str) -> bool {
    is_comment(name) || name == "open"
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
            ..
        } => {
            let line = match entry_index {
                Some(i) => match state.doc.entries.get(i) {
                    Some(entry) => Some(entry.line),
                    None => return,
                },
                // Session-level note: a session target with no line
                None => None,
            };
            let text = editor.text();
            let mut target = SessionTarget::new(&state.doc.session.id);
            if let Some(line) = line {
                target = target.with_line(line);
            }
            let target = NoteTarget::Session(target);
            let mut note = Note::new(target, "comment", NoteValue::from(text), &state.author());
            if state.annotate {
                // Annotation mode writes open codes; the fixed name
                // holds the (name, target, author) key to one per node
                note.name = "open".to_string();
            } else {
                // Notes are keyed by (name, target, author) in the DB, so a
                // literal "comment" name caps users at one comment per line.
                // Suffix with a short slice of the note's own UUID to keep the
                // key unique while the display layer renders the family name
                // "comment".
                let suffix: String = note.id.chars().take(8).collect();
                note.name = format!("comment.{suffix}");
            }
            if let Ok(()) = note::insert(db, &note) {
                let id = note.id.clone();
                state.doc.add_note(note);
                // Selection stays on the row that opened the dialog; the new
                // note rows are appended after it, so the index is unaffected.
                state.outline.add_note(entry_index, id);
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
                if let Some(owner) = state.outline.remove_note(&note_id) {
                    let target_row = match owner {
                        Some(entry_index) => state.outline.rows().iter().position(
                            |r| matches!(&r.kind, RowKind::Entry { index } if *index == entry_index),
                        ),
                        // Session notes hang off the session row, always row 0
                        None => Some(0),
                    };
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

fn draw(frame: &mut Frame, state: &mut AppState) {
    let [header_area, middle_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let short_id = state
        .doc
        .session
        .id
        .get(..8)
        .unwrap_or(&state.doc.session.id);
    let header_text = match state.doc.session.value.get("title").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => format!("{short_id} · {t}"),
        _ => short_id.to_string(),
    };
    let header = Paragraph::new(Line::from(header_text).centered()).style(styles::Panel::header());
    frame.render_widget(header, header_area);

    draw_in(frame, middle_area, state);

    let footer = Paragraph::new(footer_hint(state).centered()).style(styles::Panel::footer());
    frame.render_widget(footer, footer_area);
}

/// Draw the outline/body panes into `area`, plus any editor/confirm
/// overlay (centered on the frame). Used by the standalone app and by
/// hosts embedding the view in a dialog.
pub(crate) fn draw_in(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let [outline_area, body_area] =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).areas(area);
    draw_outline(frame, state, outline_area);
    draw_body(frame, state, body_area);
    let detail = state.outline.detail();
    let turns = state.turns.is_some();
    draw_dialog(frame, &mut state.dialog, detail, turns);
}

pub(crate) fn footer_hint(state: &AppState) -> Line<'static> {
    let mut hints: Vec<(&str, &str)> = vec![
        ("q", "quit"),
        ("Tab", "pane"),
        (
            "j/k g/G PgUp/PgDn",
            match state.focus {
                Focus::Outline => "",
                Focus::Body => "scroll",
            },
        ),
        ("n", "note"),
        ("v", "options"),
        ("o", "open"),
    ];
    if state.focus == Focus::Outline
        && let Some(note) = state.selected_note()
        && note.author == state.author()
    {
        if is_editable(&note.name) {
            hints.push(("e", "edit"));
        }
        hints.push(("d", "delete"));
    }
    hints.push(("r", "refresh"));
    hint::help_line(&hints)
}

fn draw_outline(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let active = state.focus == Focus::Outline;

    state.outline_viewport = area.height.saturating_sub(2);

    let selected = state.list_state.selected();
    let items: Vec<ListItem> = state
        .outline
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| row_to_item(row, &state.doc, Some(i) == selected, state.turns.as_deref()))
        .collect();

    let title = if state.outline.detail() {
        Line::from("Entries")
    } else {
        Line::from(vec![
            Span::raw("Entries "),
            Span::styled("(filtered)", styles::Text::dim()),
        ])
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(styles::Panel::border(active))
                .title(title),
        )
        .highlight_style(styles::Panel::selection(true));
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
        RowKind::Session => Line::from(vec![
            prefix,
            Span::styled("<Session>", styles::Text::accent()),
        ]),
        RowKind::Entry { index } => {
            let kind = doc
                .entries
                .get(*index)
                .map_or("?".to_string(), |e| e.label().to_string());
            let number_style = if is_selected {
                Style::new()
            } else {
                styles::Text::dim()
            };
            let turn = turns.and_then(|t| t.get(*index).copied().flatten());
            let mut spans = vec![
                prefix,
                Span::styled(format!("{} ", index + 1), number_style),
                Span::raw(kind.to_string()),
            ];
            if let Some(n) = turn {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    n.to_string(),
                    Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
                ));
            }
            Line::from(spans)
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
        .border_style(styles::Panel::border(active))
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
            let head = match entry_index {
                Some(i) => entry_header(doc, *i),
                None => doc.session.id.clone(),
            };
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
        styles::Text::dim(),
    ));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(header);
    lines.push(Line::from(""));
    for l in value.lines() {
        lines.push(Line::from(l.to_string()));
    }
    if let Some(metadata) = &note.metadata {
        let pretty = serde_json::from_str::<serde_json::Value>(metadata)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| metadata.clone());
        lines.push(Line::from(""));
        for l in pretty.lines() {
            lines.push(Line::from(l.to_string()));
        }
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
    // Filtered view hides the raw YAML behind the rendered message;
    // entries with no message keep it, or the pane would be blank
    let show_raw = state.outline.detail() || entry.message.is_none();
    if let Some(message) = &entry.message {
        let panel = Paragraph::new(message::render(message))
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::uniform(1)));
        sections.push(Section::from_paragraph(panel, area.width));
        if show_raw {
            sections.push(Section::from_paragraph(
                Paragraph::new(Line::from(Span::styled("--- raw ---", styles::Text::dim()))),
                area.width,
            ));
        }
    }
    if show_raw {
        sections.push(Section::from_paragraph(
            Paragraph::new(state.highlighter.highlight(&entry.yaml, "yaml"))
                .wrap(Wrap { trim: false }),
            area.width,
        ));
    }

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

    crate::stack::blit(frame, area, &virt, state.body_scroll);
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

fn draw_dialog(frame: &mut Frame, dialog: &mut Dialog, detail: bool, turns: bool) {
    match dialog {
        Dialog::None => {}
        Dialog::Options => draw_options(frame, detail, turns),
        Dialog::AddNote { editor, title, .. } => draw_editor(frame, title, editor),
        Dialog::EditNote { editor, title, .. } => draw_editor(frame, title, editor),
        Dialog::ConfirmCancel { pending } => {
            match pending.as_mut() {
                Dialog::AddNote { editor, title, .. } => draw_editor(frame, title, editor),
                Dialog::EditNote { editor, title, .. } => draw_editor(frame, title, editor),
                _ => {}
            }
            draw_confirm(frame, "Cancel your changes?");
        }
        Dialog::ConfirmDelete { .. } => {
            draw_confirm(frame, "Delete this note? This cannot be undone.")
        }
        Dialog::OpenSession(picker) => picker.draw(frame),
    }
}

/// View-options toggles: each key's label names the action it takes
/// from the current state.
fn draw_options(frame: &mut Frame, detail: bool, turns: bool) {
    let label = |on: bool| if on { "hide" } else { "show" };
    let lines = vec![
        Line::raw(format!("  d {} session detail", label(detail))).left_aligned(),
        Line::raw(format!("  t {} turns", label(turns))).left_aligned(),
    ];
    crate::dialog::draw_lines_titled(frame, Some("View options"), lines, "q back");
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
        .title(Span::styled(title.to_string(), styles::Text::dim()))
        .border_style(styles::Text::dim())
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
        styles::Text::dim(),
    )))
    .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(p, area);
}

fn compute_turns(doc: &Document) -> Vec<Option<usize>> {
    let mut counter = gage_claude::stats::TurnCounter::new();
    doc.entries
        .iter()
        .map(|e| counter.observe(&e.value))
        .collect()
}

fn scrollbar(active: bool) -> Scrollbar<'static> {
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("┃")
        .track_symbol(Some("│"))
        .style(styles::Panel::scrollbar(active))
}
