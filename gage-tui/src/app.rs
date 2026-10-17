//! TUI shell — header / outline / body / footer. Tab toggles which pane is
//! active. In the outline: j/k/g/G/PgUp/PgDn navigate, Enter or Space
//! toggles expansion (Enter on a user comment note edits it instead). In
//! the body: j/k/g/G/PgUp/PgDn scroll. `n` opens a note dialog over the
//! selected entry; `e` edits the selected user comment note; `d` deletes
//! the selected user-authored note. →/← expand and collapse (← moves to
//! the parent at a terminus). `[`/`]` are deliberately unbound so a host
//! embedding the view can step between sessions with them.

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gage_store::NoteValue;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use serde_json::Value;

use crate::doc::{Document, Note};
use crate::hint;
use crate::options::ViewOptions;
use crate::outline::{CollapseOutcome, Outline, RowKind};
use crate::picker::{self, PickColumn, PickItem, Picker, PickerAction};
use crate::session::{self, Backend};
use crate::syntax::Highlighter;
use crate::textarea::TextArea;
use crate::{message, styles};

pub fn run(
    terminal: &mut DefaultTerminal,
    doc: Option<Document>,
    line: Option<u32>,
    options: &ViewOptions,
    backend: &Backend,
) -> io::Result<()> {
    let doc = match doc {
        Some(doc) => doc,
        // No session given: the open dialog doubles as the initial
        // picker; canceling it exits the app.
        None => match standalone_pick(terminal, backend)? {
            Some(doc) => doc,
            None => return Ok(()),
        },
    };
    let mut state = AppState::new(doc, options, DocSource::Backend, backend.supports_notes());
    if let Some(line) = line {
        state.select_line(line);
    }
    loop {
        poll_pages(&mut state)?;
        terminal.draw(|frame| draw(frame, &mut state))?;
        if event::poll(EVENT_POLL)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && handle_key(&mut state, key, backend)? == KeyOutcome::Close
        {
            return Ok(());
        }
    }
}

/// Run the open dialog on a blank background until the user picks a
/// session or cancels.
fn standalone_pick(
    terminal: &mut DefaultTerminal,
    backend: &Backend,
) -> io::Result<Option<Document>> {
    let (mut picker, loader) = open_session_picker(backend, None)?;
    loop {
        loader.poll(&mut picker)?;
        terminal.draw(|frame| {
            draw_empty_shell(frame);
            picker.draw(frame);
        })?;
        if event::poll(EVENT_POLL)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match picker.handle_key(key.code) {
                PickerAction::None => {}
                PickerAction::Close => return Ok(None),
                PickerAction::Open(id) => return load_doc_blocking(&id, backend).map(Some),
                PickerAction::LoadMore => loader.start(picker.loaded()),
            }
        }
    }
}

/// How long an event loop waits for a key before it checks for a
/// landed picker page
const EVENT_POLL: Duration = Duration::from_millis(50);

/// The session-open picker with its first page loaded, and the loader
/// that fetches the pages after it.
fn open_session_picker(
    backend: &Backend,
    current: Option<&str>,
) -> io::Result<(Picker, PageLoader)> {
    let page_size = session::picker_page_size();
    let items = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(backend.list_recent(page_size, 0))
    })
    .map_err(|e| io::Error::other(e.to_string()))?;
    let has_more = items.len() == page_size;
    let picker = session_picker(backend, pick_items(backend, items), current)
        .with_paging(has_more, "Reading sessions...");
    Ok((picker, PageLoader::new(backend.clone(), page_size)))
}

/// Fetches picker pages on the runtime. The event loop polls between
/// key events and appends a page once it lands.
pub(crate) struct PageLoader {
    backend: Backend,
    page_size: usize,
    /// Where the page in flight lands
    slot: Arc<Mutex<Option<io::Result<Vec<PickItem>>>>>,
}

impl PageLoader {
    fn new(backend: Backend, page_size: usize) -> Self {
        Self {
            backend,
            page_size,
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Fetch the page starting at `offset`.
    fn start(&self, offset: usize) {
        let backend = self.backend.clone();
        let slot = Arc::clone(&self.slot);
        let page_size = self.page_size;
        tokio::spawn(async move {
            let page = backend
                .list_recent(page_size, offset)
                .await
                .map(|items| pick_items(&backend, items))
                .map_err(|e| io::Error::other(e.to_string()));
            *slot.lock().unwrap() = Some(page);
        });
    }

    /// Append the landed page, if any, to `picker`.
    fn poll(&self, picker: &mut Picker) -> io::Result<()> {
        let landed = self.slot.lock().unwrap().take();
        if let Some(page) = landed {
            let items = page?;
            let has_more = items.len() == self.page_size;
            picker.append(items, has_more);
        }
        Ok(())
    }
}

/// Append any landed picker page while the open dialog is up. Called
/// by every loop that drives the view, so a page lands without a key
/// press in the view's own loop and on the next key in a host's.
pub(crate) fn poll_pages(state: &mut AppState) -> io::Result<()> {
    if let Dialog::OpenSession { picker, loader } = &mut state.dialog {
        loader.poll(picker)?;
    }
    Ok(())
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

/// The session-open picker over `items`, its first page.
fn session_picker(backend: &Backend, items: Vec<PickItem>, current: Option<&str>) -> Picker {
    let id_heading = if backend.is_native() {
        "Native Id"
    } else {
        "Id"
    };
    let columns = vec![
        PickColumn::new(id_heading, 9),
        PickColumn::fit("Project", 24),
        PickColumn::fill("Title"),
        PickColumn::right("Modified", 8),
    ];
    Picker::new("Open session", columns, items, current)
}

/// Picker rows for one page of sessions.
fn pick_items(backend: &Backend, items: Vec<session::SessionListItem>) -> Vec<PickItem> {
    let id_style = if backend.is_native() {
        styles::Text::native_id()
    } else {
        styles::Text::id()
    };
    items
        .into_iter()
        .map(|item| PickItem {
            cells: vec![
                Span::styled(item.id_display, id_style),
                Span::raw(item.project),
                Span::raw(item.title),
                Span::styled(picker::ago(item.time_ms), styles::Text::dim()),
            ],
            id: item.id,
        })
        .collect()
}

/// Load a session document from the sync event loop.
fn load_doc_blocking(session_id: &str, backend: &Backend) -> io::Result<Document> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(backend.load(session_id))
    })
    .map_err(|e| io::Error::other(e.to_string()))
}

/// What a key press did, for hosts that embed the view. `Close` is a
/// request to dismiss the view (`q`/`Esc`); `Ignored` means the key had
/// no effect at the current position, letting a host bind its own
/// meaning (e.g. `[`/`]` stepping to another session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    backend: &Backend,
) -> io::Result<KeyOutcome> {
    poll_pages(state)?;
    match &mut state.dialog {
        Dialog::AddNote { .. } | Dialog::EditNote { .. } => {
            handle_note_dialog(state, key, backend)?;
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::ConfirmCancel { .. } => {
            handle_confirm_cancel(state, key.code);
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::ConfirmDelete { .. } => {
            handle_confirm_delete(state, key.code, backend)?;
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::OpenSession { picker, loader } => {
            match picker.handle_key(key.code) {
                PickerAction::None => {}
                PickerAction::Close => state.dialog = Dialog::None,
                PickerAction::Open(id) => {
                    state.dialog = Dialog::None;
                    if id != state.doc.session.id {
                        let doc = load_doc_blocking(&id, backend)?;
                        state.replace_doc(doc);
                    }
                }
                PickerAction::LoadMore => loader.start(picker.loaded()),
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::NoteType { entry_index } => {
            let entry_index = *entry_index;
            match key.code {
                KeyCode::Char('c') => {
                    state.open_add_editor(entry_index, "comment", "Add comment");
                }
                KeyCode::Char('o') => {
                    state.open_add_editor(entry_index, "open", "Add open code");
                }
                KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Esc => {
                    state.dialog = Dialog::None;
                }
                _ => {}
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::Options => {
            match key.code {
                KeyCode::Char('a') => state.toggle_assistant(),
                KeyCode::Char('u') => state.toggle_user(),
                KeyCode::Char('t') => state.toggle_tools(),
                KeyCode::Char('e') => state.toggle_all_entries(),
                KeyCode::Char('d') => state.toggle_details(),
                KeyCode::Char('n') => state.toggle_turns(),
                KeyCode::Enter | KeyCode::Esc => {
                    state.dialog = Dialog::None;
                }
                _ => {}
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::NotesUnavailable => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc) {
                state.dialog = Dialog::None;
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
        KeyCode::Char('g') | KeyCode::Home => {
            match state.focus {
                Focus::Outline => state.select_first(),
                Focus::Body => state.body_scroll_to_top(),
            }
            KeyOutcome::Consumed
        }
        KeyCode::Char('G') | KeyCode::End => {
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
            if state.notes_writable {
                state.begin_add_note();
            } else {
                state.dialog = Dialog::NotesUnavailable;
            }
            KeyOutcome::Consumed
        }
        KeyCode::Char('v') => {
            state.dialog = Dialog::Options;
            KeyOutcome::Consumed
        }
        KeyCode::Char('o') => {
            let (picker, loader) = open_session_picker(backend, Some(&state.doc.session.id))?;
            state.dialog = Dialog::OpenSession { picker, loader };
            KeyOutcome::Consumed
        }
        KeyCode::Char('e') if state.notes_writable => {
            state.begin_edit_note();
            KeyOutcome::Consumed
        }
        KeyCode::Char('d') if state.notes_writable => {
            state.begin_delete_note();
            KeyOutcome::Consumed
        }
        KeyCode::Char('r') => {
            state.reload(backend)?;
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
    /// The backend the view runs over (`Backend::load`)
    Backend,
}

/// Snapshot of the view's UI position, for hosts that re-open sessions
/// and restore where the user left off. Selection is saved by row
/// identity, and the tree's expansion state alongside it, so the
/// restore lands on the same node even though the host rebuilds the
/// view (and its outline) from scratch.
#[derive(Clone)]
pub(crate) struct SavedUi {
    selected: Option<RowKind>,
    offset: usize,
    body_scroll: u16,
    focus_body: bool,
    session_expanded: bool,
    entry_expanded: HashSet<usize>,
}

enum Dialog {
    None,
    /// Note-type chooser (`n`), layered before the add editor
    NoteType {
        /// Target entry; None for a session-level note (the
        /// `<Session>` row or one of its notes selected)
        entry_index: Option<usize>,
    },
    AddNote {
        /// Target entry; None for a session-level note (the
        /// `<Session>` row or one of its notes selected)
        entry_index: Option<usize>,
        editor: TextArea,
        /// Dialog caption, fixed at open per the note kind
        title: &'static str,
        /// Note name to write, fixed at open per the note kind
        name: &'static str,
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
    /// Session-open picker (`o`) and the loader for its later pages
    OpenSession {
        picker: Picker,
        loader: PageLoader,
    },
    /// View-options toggles (`v`)
    Options,
    /// Notice that `n` has nothing to write to over a native source
    NotesUnavailable,
}

const NOTES_UNAVAILABLE: &str = "Notes cannot be added to native sessions. \
Add the session to the store to write notes for it.";

fn new_editor(text: &str) -> TextArea {
    TextArea::new(text)
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
    /// A centering requested before the first draw, applied once the
    /// outline viewport is known
    center_pending: bool,
    highlighter: Highlighter,
    dialog: Dialog,
    view: ViewOptions,
    turns: Option<Vec<Option<usize>>>,
    source: DocSource,
    /// Whether the backend writes notes; gates the note keys
    notes_writable: bool,
}

impl AppState {
    pub(crate) fn new(
        doc: Document,
        options: &ViewOptions,
        source: DocSource,
        notes_writable: bool,
    ) -> Self {
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let outline = Outline::new(
            session_note_ids,
            entry_note_ids,
            entry_hidden(&doc, options),
        );
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let turns = options.turns.then(|| compute_turns(&doc));
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
            center_pending: false,
            highlighter: Highlighter::new(),
            dialog: Dialog::None,
            view: options.clone(),
            turns,
            source,
            notes_writable,
        }
    }

    pub(crate) fn save_ui(&self) -> SavedUi {
        let (session_expanded, entry_expanded) = self.outline.expansion();
        SavedUi {
            selected: self
                .list_state
                .selected()
                .and_then(|i| self.outline.row(i))
                .map(|r| r.kind.clone()),
            offset: self.list_state.offset(),
            body_scroll: self.body_scroll,
            focus_body: self.focus == Focus::Body,
            session_expanded,
            entry_expanded,
        }
    }

    /// Re-apply a [`SavedUi`]: seed the outline's expansion state, then
    /// find the saved row by identity in the rebuilt rows. When the row
    /// is gone (e.g. its note was deleted between views), selection
    /// falls back to the top and the saved body position is dropped.
    pub(crate) fn restore_ui(&mut self, saved: &SavedUi) {
        self.outline
            .set_expansion(saved.session_expanded, saved.entry_expanded.clone());
        let found = saved.selected.as_ref().and_then(|kind| {
            self.outline
                .rows()
                .iter()
                .position(|r| same_row(&r.kind, kind))
        });
        self.list_state.select(found.or(Some(0)));
        *self.list_state.offset_mut() = saved.offset.min(self.outline.len().saturating_sub(1));
        if found.is_some() {
            self.body_scroll = saved.body_scroll;
            self.focus = if saved.focus_body {
                Focus::Body
            } else {
                Focus::Outline
            };
        } else {
            self.body_scroll = 0;
            self.focus = Focus::Outline;
        }
    }

    /// Select the entry at JSONL line `line`, or the nearest entry
    /// before it, for hosts opening the view at a session line. A line
    /// before the first entry leaves the selection where it is.
    pub(crate) fn select_line(&mut self, line: u32) {
        if let Some(index) = self.doc.entry_index_for_line(line) {
            self.select_entry(index);
        }
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

    fn reload(&mut self, backend: &Backend) -> io::Result<()> {
        let session_id = self.doc.session.id.clone();
        let prior = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))
            .map(|r| r.kind.clone());
        let doc = match &self.source {
            DocSource::Backend => tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(backend.load(&session_id))
            }),
        }
        .map_err(|e| io::Error::other(e.to_string()))?;
        if self.turns.is_some() {
            self.turns = Some(compute_turns(&doc));
        }
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let hidden = entry_hidden(&doc, &self.view);
        self.doc = doc;
        let pane_row = self.selected_pane_row();
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
        self.select_at_pane_row(new_sel, pane_row);
        Ok(())
    }

    /// The selected row's distance from the top of the outline pane
    fn selected_pane_row(&self) -> usize {
        self.list_state
            .selected()
            .map_or(0, |sel| sel.saturating_sub(self.list_state.offset()))
    }

    /// Select `row` and scroll so it sits `pane_row` rows from the top
    /// of the pane, as the prior selection did before a rebuild. A row
    /// with fewer rows above it than `pane_row` sits below the first
    /// row instead.
    fn select_at_pane_row(&mut self, row: Option<usize>, pane_row: usize) {
        self.list_state.select(row);
        *self.list_state.offset_mut() = row.map_or(0, |r| r.saturating_sub(pane_row));
    }

    fn toggle_assistant(&mut self) {
        self.view.assistant = !self.view.assistant;
        self.view.all_entries = false;
        self.apply_filter();
    }

    fn toggle_user(&mut self) {
        self.view.user = !self.view.user;
        self.view.all_entries = false;
        self.apply_filter();
    }

    fn toggle_tools(&mut self) {
        self.view.tools = !self.view.tools;
        self.view.all_entries = false;
        self.apply_filter();
    }

    fn toggle_all_entries(&mut self) {
        self.view.all_entries = !self.view.all_entries;
        self.apply_filter();
    }

    /// Recompute the membership mask and reapply it, keeping the selection
    /// and its place in the pane when its row survives the rebuild.
    fn apply_filter(&mut self) {
        let prior = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))
            .map(|r| r.kind.clone());
        let pane_row = self.selected_pane_row();
        self.outline.set_hidden(entry_hidden(&self.doc, &self.view));
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
        self.select_at_pane_row(new_sel, pane_row);
    }

    /// Toggle raw entry detail in the content pane. Independent of the
    /// tree filter.
    fn toggle_details(&mut self) {
        self.view.details = !self.view.details;
    }

    fn toggle_turns(&mut self) {
        self.view.turns = !self.view.turns;
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
        let hidden = entry_hidden(&doc, &self.view);
        self.doc = doc;
        self.outline
            .reload(session_note_ids, entry_note_ids, hidden);
        self.list_state.select(Some(0));
        *self.list_state.offset_mut() = 0;
        self.body_scroll = 0;
        self.focus = Focus::Outline;
        self.source = DocSource::Backend;
    }

    /// Scroll so the selected row sits at the middle of the outline.
    /// Before the first draw the viewport is unknown, and the
    /// centering is applied by that draw instead.
    fn center_selected(&mut self) {
        if self.outline_viewport == 0 {
            self.center_pending = true;
            return;
        }
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
        self.dialog = Dialog::NoteType { entry_index };
    }

    fn open_add_editor(
        &mut self,
        entry_index: Option<usize>,
        name: &'static str,
        title: &'static str,
    ) {
        self.dialog = Dialog::AddNote {
            entry_index,
            editor: new_editor(""),
            title,
            name,
        };
    }

    fn begin_edit_note(&mut self) -> bool {
        let Some(note) = self.selected_note() else {
            return false;
        };
        if note.author != self.author() || !is_editable(&note.name) {
            return false;
        }
        let title = if note.name == "open" {
            "Edit open code"
        } else {
            "Edit comment"
        };
        let (note_id, text) = (note.id.clone(), note.text());
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

/// Per-entry filter mask: `true` marks an entry the membership options
/// exclude from the outline.
fn entry_hidden(doc: &Document, view: &ViewOptions) -> Vec<bool> {
    doc.entries
        .iter()
        .map(|e| !entry_visible(e.label(), view))
        .collect()
}

/// Whether an entry's outline label is admitted by the membership options.
/// `all_entries` admits every label; otherwise the three message groups
/// select their labels and everything else (meta, summary, system, and
/// bookkeeping types) stays hidden.
fn entry_visible(label: &str, view: &ViewOptions) -> bool {
    if view.all_entries {
        return true;
    }
    match label {
        "assistant" | "thinking" => view.assistant,
        "user" => view.user,
        "tool_use" | "tool_result" => view.tools,
        _ => false,
    }
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

/// User-editable note names: comments and open codes.
fn is_editable(name: &str) -> bool {
    matches!(name, "comment" | "open")
}

fn handle_note_dialog(state: &mut AppState, key: KeyEvent, backend: &Backend) -> io::Result<()> {
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
                commit_note(state, backend)?;
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
                _ => return Ok(()),
            };
            if is_newline_shortcut {
                editor.insert_newline();
            } else {
                editor.input(key);
            }
        }
    }
    Ok(())
}

/// Write the editor's note through the backend and reflect it in the
/// document. A write failure is returned; the dialog is already
/// closed, so the host surfaces the error and the view stays as it
/// was.
fn commit_note(state: &mut AppState, backend: &Backend) -> io::Result<()> {
    match std::mem::replace(&mut state.dialog, Dialog::None) {
        Dialog::AddNote {
            entry_index,
            editor,
            name,
            ..
        } => {
            let line = match entry_index {
                Some(i) => match state.doc.entries.get(i) {
                    Some(entry) => Some(entry.line),
                    None => return Ok(()),
                },
                // Session-level note: a session target with no line
                None => None,
            };
            let note = backend
                .add_note(
                    &state.doc.session.id,
                    line,
                    name,
                    editor.text(),
                    &state.author(),
                )
                .map_err(|e| io::Error::other(e.to_string()))?;
            let id = note.id.clone();
            state.doc.add_note(note);
            // Selection stays on the row that opened the dialog; the new
            // note rows are appended after it, so the index is unaffected.
            state.outline.add_note(entry_index, id);
        }
        Dialog::EditNote {
            note_id, editor, ..
        } => {
            let note = backend
                .edit_note(&note_id, editor.text())
                .map_err(|e| io::Error::other(e.to_string()))?;
            state
                .doc
                .replace_note_value(&note_id, note.value, note.modified_ms);
        }
        other => state.dialog = other,
    }
    Ok(())
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

fn handle_confirm_delete(state: &mut AppState, code: KeyCode, backend: &Backend) -> io::Result<()> {
    let Dialog::ConfirmDelete { note_id } = std::mem::replace(&mut state.dialog, Dialog::None)
    else {
        return Ok(());
    };
    if !matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
        return Ok(());
    }
    backend
        .delete_note(&note_id)
        .map_err(|e| io::Error::other(e.to_string()))?;
    state.doc.remove_note(&note_id);
    if let Some(owner) = state.outline.remove_note(&note_id) {
        let target_row =
            match owner {
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
    Ok(())
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
    draw_dialog(frame, &mut state.dialog, &state.view);
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
    if state.center_pending {
        state.center_pending = false;
        state.center_selected();
    }

    let selected = state.list_state.selected();
    let items: Vec<ListItem> = state
        .outline
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| row_to_item(row, &state.doc, Some(i) == selected, state.turns.as_deref()))
        .collect();

    let title = if state.view.all_entries {
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
    let value = match &note.value {
        NoteValue::Text(s) => s.clone(),
        NoteValue::Json(v) => serde_json::to_string_pretty(v).unwrap(),
    };
    let modified_suffix = if note.modified_ms > note.created_ms {
        format!(" · edited {}", format_ms(note.modified_ms))
    } else {
        String::new()
    };
    let mut header_spans = vec![Span::styled(
        format!(
            "{} · {}{}",
            note.author,
            format_ms(note.created_ms),
            modified_suffix
        ),
        styles::Text::dim(),
    )];
    if let Some(n) = note.span_lines() {
        header_spans.push(Span::styled(" · ", styles::Text::dim()));
        header_spans.push(Span::styled(
            format!("spans {n} lines"),
            Style::new().add_modifier(Modifier::REVERSED),
        ));
    }
    let header = Line::from(header_spans);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(header);
    lines.push(Line::from(""));
    for l in value.lines() {
        lines.push(Line::from(l.to_string()));
    }
    if let Some(metadata) = &note.metadata {
        let pretty = serde_json::to_string_pretty(metadata).unwrap();
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
    let show_raw = state.view.details || entry.message.is_none();
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

fn draw_dialog(frame: &mut Frame, dialog: &mut Dialog, view: &ViewOptions) {
    match dialog {
        Dialog::None => {}
        Dialog::Options => draw_options(frame, view),
        Dialog::NotesUnavailable => {
            crate::dialog::draw_wrapped_message(frame, NOTES_UNAVAILABLE, "Enter dismiss")
        }
        Dialog::NoteType { .. } => draw_note_type(frame),
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
        Dialog::OpenSession { picker, .. } => picker.draw(frame),
    }
}

/// View-options menu: each key toggles one option; the checkbox shows
/// whether it is on. While "All entries" is on the membership checkboxes
/// render dim, since the filter is overridden, but their keys still
/// toggle.
fn draw_options(frame: &mut Frame, view: &ViewOptions) {
    let dim = view.all_entries;
    let lines = vec![
        Line::raw("Entry types:"),
        Line::raw(""),
        option_line('a', view.assistant, dim, "Assistant messages"),
        option_line('u', view.user, dim, "User messages"),
        option_line('t', view.tools, dim, "Tool calls"),
        option_line('e', view.all_entries, false, "All entries"),
        Line::raw(""),
        Line::raw("Additional info:"),
        Line::raw(""),
        option_line('d', view.details, false, "Entry details"),
        option_line('n', view.turns, false, "Turns"),
    ];
    let hint = Line::from(vec![
        Span::styled("Enter", styles::Dialog::key()),
        Span::raw(" close"),
    ]);
    crate::dialog::draw_menu(frame, "View options", lines, hint);
}

/// One menu row: the key in the key color, a checkbox, and the label.
/// When `dim_box` is set the whole checkbox renders dim to mark the
/// option as overridden, while its mark still reads.
fn option_line(key: char, on: bool, dim_box: bool, label: &str) -> Line<'static> {
    let mark = if on { "x" } else { " " };
    let checkbox = if dim_box {
        Span::styled(format!("[{mark}]"), styles::Dialog::dim())
    } else {
        Span::raw(format!("[{mark}]"))
    };
    Line::from(vec![
        Span::styled(key.to_string(), styles::Dialog::key()),
        Span::raw(" "),
        checkbox,
        Span::raw(format!(" {label}")),
    ])
}

/// Note-type chooser: which kind of note `n` will add.
fn draw_note_type(frame: &mut Frame) {
    let lines = vec![
        Line::raw("  c comment (general note)").left_aligned(),
        Line::raw("  o open code (qualitative analysis)").left_aligned(),
    ];
    crate::dialog::draw_lines_titled(frame, Some("Note type"), lines, "q cancel");
}

const EDITOR_HINT: &str = "Enter save · Shift/Alt+Enter or Ctrl+J newline · Esc cancel";

fn draw_editor(frame: &mut Frame, title: &str, editor: &mut TextArea) {
    crate::dialog::draw_editor(frame, title, None, editor, EDITOR_HINT);
}

fn draw_confirm(frame: &mut Frame, message: &str) {
    crate::dialog::draw_wrapped_message(frame, message, "y / n");
}

fn compute_turns(doc: &Document) -> Vec<Option<usize>> {
    let mut counter = crate::format::TurnCounter::new();
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEvent;

    use super::*;
    use crate::doc::{Entry, Session};
    use crate::session::tests::fixture;

    /// The rendered screen as one string, rows joined by newlines
    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn press(state: &mut AppState, backend: &Backend, code: KeyCode) -> KeyOutcome {
        handle_key(state, KeyEvent::new(code, KeyModifiers::NONE), backend).unwrap()
    }

    /// Background color of the first `ch` in the first row that contains
    /// `marker`, for asserting a cell's style.
    fn cell_bg(terminal: &Terminal<TestBackend>, marker: &str, ch: char) -> ratatui::style::Color {
        let buffer = terminal.backend().buffer();
        let needle = ch.to_string();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if !row.contains(marker) {
                continue;
            }
            for x in 0..buffer.area.width {
                if buffer[(x, y)].symbol() == needle {
                    return buffer[(x, y)].bg;
                }
            }
        }
        panic!("no '{ch}' in a row containing {marker:?}");
    }

    fn type_text(state: &mut AppState, backend: &Backend, text: &str) {
        for c in text.chars() {
            press(state, backend, KeyCode::Char(c));
        }
    }

    /// The labels of the outline's visible rows
    fn outline_labels(state: &AppState) -> Vec<String> {
        state
            .outline
            .rows()
            .iter()
            .map(|r| match &r.kind {
                RowKind::Session => "<Session>".to_string(),
                RowKind::Entry { index } => format!("entry {index}"),
                RowKind::Note { note_id, .. } => {
                    format!("note {}", state.doc.note(note_id).unwrap().text())
                }
            })
            .collect()
    }

    /// Adding, editing, and deleting a note through the keys writes
    /// the store and keeps the outline in step with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn select_line_opens_at_the_entry_for_a_session_line() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 1"]);
        assert_eq!(state.selected_entry_index(), None);

        // The fixture's entries sit at lines 1 and 2
        state.select_line(2);
        assert_eq!(state.selected_entry_index(), Some(1));
        state.select_line(1);
        assert_eq!(state.selected_entry_index(), Some(0));

        // A line past the last entry selects the nearest entry before it
        state.select_line(50);
        assert_eq!(state.selected_entry_index(), Some(1));

        // A line before the first entry leaves the selection alone
        state.select_line(0);
        assert_eq!(state.selected_entry_index(), Some(1));
    }

    /// A document of `count` user entries, one per JSONL line
    fn doc_with_entries(count: u32) -> Document {
        Document {
            session: Session {
                id: "s1".into(),
                value: serde_json::json!({}),
            },
            entries: (1..=count)
                .map(|line| Entry {
                    line,
                    uuid: Some(format!("u{line}")),
                    entry_type: "user".into(),
                    message_subtype: Some("text".into()),
                    value: serde_json::json!({"type": "user"}),
                })
                .collect(),
            notes: Vec::new(),
        }
    }

    /// A document of `count` entries alternating user and assistant,
    /// one per JSONL line
    fn doc_with_alternating_entries(count: u32) -> Document {
        let mut doc = doc_with_entries(count);
        for entry in doc.entries.iter_mut().skip(1).step_by(2) {
            entry.entry_type = "assistant".into();
        }
        doc
    }

    #[test]
    fn filter_toggle_keeps_the_selected_row_in_its_pane_position() {
        let doc = doc_with_alternating_entries(60);
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        // Entry 40 is an user entry (even index); open on it centered
        state.select_line(41);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert_eq!(state.selected_entry_index(), Some(40));
        let pane_row = state.selected_pane_row();
        assert!(pane_row > 0);

        // Hiding the assistant entries halves the rows above; the
        // selected entry keeps its distance from the top of the pane
        state.toggle_assistant();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert_eq!(state.selected_entry_index(), Some(40));
        assert_eq!(state.selected_pane_row(), pane_row);

        state.toggle_assistant();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert_eq!(state.selected_entry_index(), Some(40));
        assert_eq!(state.selected_pane_row(), pane_row);
    }

    #[test]
    fn filter_toggle_moves_a_row_up_when_too_few_rows_remain_above_it() {
        let doc = doc_with_alternating_entries(60);
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        // Entry 10 sits at row 11 (after the session row); scrolled so
        // that it is the eighth row of the pane
        state.select_entry(10);
        *state.list_state.offset_mut() = 4;
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert_eq!(state.selected_pane_row(), 7);

        // Hiding the assistant entries leaves six rows above the
        // selection, fewer than its pane row, so the list starts at
        // its first row with no gap above
        state.toggle_assistant();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert_eq!(state.selected_entry_index(), Some(10));
        assert_eq!(state.list_state.offset(), 0);
        assert_eq!(state.selected_pane_row(), 6);
    }

    #[test]
    fn select_line_before_the_first_draw_centers_on_that_draw() {
        let doc = doc_with_entries(60);
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        state.select_line(50);
        let selected = state.list_state.selected().unwrap();
        assert_eq!(state.selected_entry_index(), Some(49));
        assert_eq!(state.list_state.offset(), 0);
        assert!(state.center_pending);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let half = usize::from(state.outline_viewport / 2);
        assert!(half > 0);
        assert_eq!(state.list_state.offset(), selected - half);
        assert!(!state.center_pending);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn note_keys_write_the_store() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 1"]);

        // Add a comment on the second entry
        press(&mut state, &backend, KeyCode::Char('j'));
        press(&mut state, &backend, KeyCode::Char('j'));
        press(&mut state, &backend, KeyCode::Char('n'));
        press(&mut state, &backend, KeyCode::Char('c'));
        type_text(&mut state, &backend, "first");
        press(&mut state, &backend, KeyCode::Enter);
        assert!(matches!(state.dialog, Dialog::None));
        press(&mut state, &backend, KeyCode::Right);
        assert_eq!(
            outline_labels(&state),
            ["<Session>", "entry 0", "entry 1", "note first"]
        );
        let stored = backend.load(&f.session_id).await.unwrap();
        assert_eq!(stored.notes_for_line(2)[0].text(), "first");
        assert_eq!(stored.notes[0].author, state.author());

        // Edit it: the editor opens with the text, Enter saves
        press(&mut state, &backend, KeyCode::Char('j'));
        press(&mut state, &backend, KeyCode::Char('e'));
        assert!(matches!(state.dialog, Dialog::EditNote { .. }));
        type_text(&mut state, &backend, " second");
        press(&mut state, &backend, KeyCode::Enter);
        assert_eq!(outline_labels(&state)[3], "note first second");
        let stored = backend.load(&f.session_id).await.unwrap();
        assert_eq!(stored.notes[0].text(), "first second");

        // Delete it after confirming
        press(&mut state, &backend, KeyCode::Char('d'));
        assert!(matches!(state.dialog, Dialog::ConfirmDelete { .. }));
        press(&mut state, &backend, KeyCode::Char('y'));
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 1"]);
        let stored = backend.load(&f.session_id).await.unwrap();
        assert!(stored.notes.is_empty());
    }

    /// Over a native source `n` explains that notes need a stored
    /// session, and the edit and delete keys are inert.
    #[tokio::test(flavor = "multi_thread")]
    async fn note_key_explains_itself_without_note_support() {
        let f = fixture();
        let backend = Backend::native(&f.spec).await.unwrap();
        let doc = backend.load(&f.native_id).await.unwrap();
        let mut state = AppState::new(
            doc,
            &ViewOptions::default(),
            DocSource::Backend,
            backend.supports_notes(),
        );
        assert_eq!(
            press(&mut state, &backend, KeyCode::Char('n')),
            KeyOutcome::Consumed
        );
        assert!(matches!(state.dialog, Dialog::NotesUnavailable));
        press(&mut state, &backend, KeyCode::Enter);
        assert!(matches!(state.dialog, Dialog::None));
        assert_eq!(
            press(&mut state, &backend, KeyCode::Char('e')),
            KeyOutcome::Ignored
        );
        assert_eq!(
            press(&mut state, &backend, KeyCode::Char('d')),
            KeyOutcome::Ignored
        );
    }

    /// The note editor and the delete confirm draw through the shared
    /// dialog chrome: title in the border, hint on the row below it.
    #[tokio::test(flavor = "multi_thread")]
    async fn note_dialogs_use_the_shared_chrome() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        press(&mut state, &backend, KeyCode::Char('n'));
        press(&mut state, &backend, KeyCode::Char('c'));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Add comment"), "{text}");
        assert!(text.contains("Enter save"), "{text}");

        press(&mut state, &backend, KeyCode::Esc);
        backend
            .add_note(&f.session_id, None, "comment", "x".into(), &state.author())
            .unwrap();
        state.reload(&backend).unwrap();
        press(&mut state, &backend, KeyCode::Right);
        press(&mut state, &backend, KeyCode::Char('j'));
        press(&mut state, &backend, KeyCode::Char('d'));
        assert!(matches!(state.dialog, Dialog::ConfirmDelete { .. }));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Delete this note?"), "{text}");
        assert!(text.contains("y / n"), "{text}");
    }

    #[test]
    fn membership_maps_labels_to_groups() {
        let mut v = ViewOptions::default();
        assert!(v.assistant && v.user && !v.tools && !v.all_entries);
        assert!(entry_visible("assistant", &v));
        assert!(entry_visible("thinking", &v));
        assert!(entry_visible("user", &v));
        assert!(!entry_visible("tool_use", &v));
        assert!(!entry_visible("tool_result", &v));
        for other in ["meta", "summary", "system", "api_error", "queue-operation"] {
            assert!(!entry_visible(other, &v), "{other}");
        }
        v.tools = true;
        assert!(entry_visible("tool_use", &v));
        assert!(entry_visible("tool_result", &v));
    }

    #[test]
    fn all_entries_admits_every_label() {
        let v = ViewOptions {
            assistant: false,
            user: false,
            tools: false,
            all_entries: true,
            details: false,
            turns: false,
        };
        for label in [
            "user",
            "meta",
            "summary",
            "system",
            "api_error",
            "atis-latch",
        ] {
            assert!(entry_visible(label, &v), "{label}");
        }
    }

    /// The `v` menu lists the toggles with their checkbox state, and a
    /// toggle key flips the box without closing the menu.
    #[tokio::test(flavor = "multi_thread")]
    async fn view_menu_shows_checkboxes_and_toggles_in_place() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        press(&mut state, &backend, KeyCode::Char('v'));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("a [x] Assistant messages"), "{text}");
        assert!(text.contains("t [ ] Tool calls"), "{text}");
        assert!(text.contains("d [ ] Entry details"), "{text}");
        assert!(text.contains("Enter close"), "{text}");
        // Two spaces inside the border, and option rows are not indented
        // under their headers: both sit two columns in from the border.
        assert!(text.contains("│  Entry types:"), "{text}");
        assert!(text.contains("│  a [x] Assistant messages"), "{text}");
        assert!(!text.contains("│   a [x]"), "{text}");

        press(&mut state, &backend, KeyCode::Char('t'));
        assert!(matches!(state.dialog, Dialog::Options));
        assert!(state.view.tools);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert!(screen(&terminal).contains("t [x] Tool calls"));

        // "All entries" keeps each membership checkbox visible rather
        // than replacing it, so the stored state still reads.
        press(&mut state, &backend, KeyCode::Char('e'));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("e [x] All entries"), "{text}");
        assert!(text.contains("a [x] Assistant messages"), "{text}");
        assert!(!text.contains('~'), "{text}");

        // Enter closes the menu.
        press(&mut state, &backend, KeyCode::Enter);
        assert!(matches!(state.dialog, Dialog::None));
    }

    /// The menu keys use the key color, the hint's "Enter" is a key and
    /// "close" is normal, and an overridden checkbox renders dim.
    #[tokio::test(flavor = "multi_thread")]
    async fn view_menu_styles_keys_hint_and_overridden_boxes() {
        use ratatui::style::Color;

        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('e'));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        // The key color is cyan on the reversed surface.
        assert_eq!(cell_bg(&terminal, "Assistant messages", 'a'), Color::Cyan);
        // The hint splits into a cyan key and a plain label.
        assert_eq!(cell_bg(&terminal, "Enter close", 'E'), Color::Cyan);
        assert_ne!(cell_bg(&terminal, "Enter close", 'c'), Color::Cyan);
        // "All entries" dims the other checkboxes whole, bracket and
        // mark alike, but not its own.
        assert_eq!(
            cell_bg(&terminal, "Assistant messages", '['),
            Color::DarkGray
        );
        assert_eq!(
            cell_bg(&terminal, "Assistant messages", 'x'),
            Color::DarkGray
        );
        assert_ne!(cell_bg(&terminal, "All entries", '['), Color::DarkGray);
    }

    /// Toggling a membership option releases "All entries"; the content
    /// detail and turns toggles leave membership untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn toggling_a_type_releases_all_entries() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);

        state.toggle_all_entries();
        assert!(state.view.all_entries);
        state.toggle_assistant();
        assert!(!state.view.all_entries);
        assert!(!state.view.assistant);

        state.toggle_all_entries();
        assert!(state.view.all_entries);
        state.toggle_details();
        assert!(state.view.details);
        assert!(
            state.view.all_entries,
            "details is independent of membership"
        );
    }

    /// Turning off the user type hides the user entry from the tree.
    #[tokio::test(flavor = "multi_thread")]
    async fn toggling_user_off_hides_the_user_entry() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 1"]);
        state.toggle_user();
        assert_eq!(outline_labels(&state), ["<Session>", "entry 1"]);
    }

    /// The notice wraps inside a terminal narrower than its text, so
    /// every word is on screen and the hint sits below it.
    #[tokio::test(flavor = "multi_thread")]
    async fn notes_unavailable_notice_wraps_to_the_terminal() {
        let f = fixture();
        let backend = Backend::native(&f.spec).await.unwrap();
        let doc = backend.load(&f.native_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        press(&mut state, &backend, KeyCode::Char('n'));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        let rows: Vec<&str> = text.lines().collect();
        let first = rows
            .iter()
            .position(|r| r.contains("Notes cannot be added"))
            .expect("the notice should be on screen");
        assert!(rows[first + 1].contains("to the store"), "{text}");
        assert!(rows.iter().any(|r| r.contains("Enter dismiss")), "{text}");
    }
}
