//! TUI shell — header / outline / body / footer. Tab toggles which pane is
//! active. In the outline: j/k/g/G/PgUp/PgDn navigate, Enter or Space
//! toggles expansion (Enter on a user comment note edits it instead). In
//! the body: j/k/g/G/PgUp/PgDn scroll. `n` opens a note dialog over the
//! selected entry; `e` edits the selected user comment note; `d` deletes
//! the selected user-authored note. →/← expand and collapse (← moves to
//! the parent at a terminus). `[`/`]` are deliberately unbound so a host
//! embedding the view can step between sessions with them.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gage_session::INVALID_ENTRY_TYPE;
use gage_store::{ChangeKind, NoteValue};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::attrs::attr_lines;
use crate::doc::{Document, Dropped, EntryBody, Note, Redaction, SessionAttrs};
use crate::hint;
use crate::input::spawn_input_thread;
use crate::options::ViewOptions;
use crate::outline::{AttachmentFiles, CollapseOutcome, Expansion, Outline, RowKind};
use crate::picker::{self, PickColumn, PickItem, Picker, PickerAction};
use crate::session::{self, Backend};
use crate::textarea::TextArea;
use crate::{
    attachment, invalid_entry, json, message, selection, session_attachment, styles, tool,
};

pub async fn run(
    terminal: &mut DefaultTerminal,
    doc: Option<Document>,
    line: Option<u32>,
    options: &ViewOptions,
    backend: &Backend,
    enhanced_keys: bool,
) -> io::Result<()> {
    let stop_input = Arc::new(AtomicBool::new(false));
    let (events, view_events) = unbounded_channel();
    let mut inbox = Inbox {
        input: spawn_input_thread(Arc::clone(&stop_input)),
        events,
        view_events,
    };
    let outcome = run_in(
        terminal,
        doc,
        line,
        options,
        backend,
        enhanced_keys,
        &mut inbox,
    )
    .await;
    stop_input.store(true, Ordering::Relaxed);
    outcome
}

/// What a view loop blocks on. The loop draws once per message and
/// otherwise sleeps.
struct Inbox {
    /// Terminal events from the input thread
    input: UnboundedReceiver<Event>,
    /// The sender the page loader is given
    events: UnboundedSender<ViewEvent>,
    /// The loader's events
    view_events: UnboundedReceiver<ViewEvent>,
}

impl Inbox {
    /// The next loader event. The inbox holds a sender, so the
    /// channel never closes.
    fn view_event(&self, received: Option<ViewEvent>) -> ViewEvent {
        received.expect("inbox holds a view event sender")
    }
}

async fn run_in(
    terminal: &mut DefaultTerminal,
    doc: Option<Document>,
    line: Option<u32>,
    options: &ViewOptions,
    backend: &Backend,
    enhanced_keys: bool,
    inbox: &mut Inbox,
) -> io::Result<()> {
    let doc = match doc {
        Some(doc) => doc,
        // No session given: the open dialog doubles as the initial
        // picker; canceling it exits the app.
        None => match standalone_pick(terminal, backend, inbox).await? {
            Some(doc) => doc,
            None => return Ok(()),
        },
    };
    let mut state = AppState::new(doc, options, DocSource::Backend, backend.supports_notes());
    state.enhanced_keys = enhanced_keys;
    if let Some(line) = line {
        state.select_line(line);
    }
    loop {
        terminal.draw(|frame| draw(frame, &mut state))?;
        tokio::select! {
            ev = inbox.input.recv() => match ev {
                Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if handle_key(&mut state, key, backend, &inbox.events)? == KeyOutcome::Close {
                        return Ok(());
                    }
                }
                // A resize or another terminal event: redraw
                Some(_) => {}
                // Terminal input is gone; without keys the view can
                // never be closed, so exit rather than trap the user.
                None => return Ok(()),
            },
            ev = inbox.view_events.recv() => apply_event(&mut state, inbox.view_event(ev))?,
        }
    }
}

/// Run the open dialog on a blank background until the user picks a
/// session or cancels.
async fn standalone_pick(
    terminal: &mut DefaultTerminal,
    backend: &Backend,
    inbox: &mut Inbox,
) -> io::Result<Option<Document>> {
    let (mut picker, loader) = open_session_picker(backend, None, inbox.events.clone())?;
    loop {
        terminal.draw(|frame| {
            draw_empty_shell(frame);
            picker.draw(frame);
        })?;
        tokio::select! {
            ev = inbox.input.recv() => match ev {
                Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    match picker.handle_key(key.code) {
                        PickerAction::None => {}
                        PickerAction::Close => return Ok(None),
                        PickerAction::Open(id) => {
                            return backend
                                .load(&id)
                                .await
                                .map(Some)
                                .map_err(|e| io::Error::other(e.to_string()));
                        }
                        PickerAction::LoadMore => loader.start(picker.loaded()),
                    }
                }
                Some(_) => {}
                None => return Ok(None),
            },
            ev = inbox.view_events.recv() => match inbox.view_event(ev) {
                ViewEvent::Page(page) => {
                    let page = page?;
                    picker.append(page.items, page.has_more);
                }
                ViewEvent::LoadingDue => {}
            },
        }
    }
}

/// The session-open picker with its first page loaded, and the loader
/// that fetches the pages after it.
fn open_session_picker(
    backend: &Backend,
    current: Option<&str>,
    events: UnboundedSender<ViewEvent>,
) -> io::Result<(Picker, PageLoader)> {
    let page_size = session::picker_page_size();
    let items = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(backend.list_recent(page_size, 0))
    })
    .map_err(|e| io::Error::other(e.to_string()))?;
    let has_more = items.len() == page_size;
    let picker = session_picker(backend, pick_items(backend, items), current)
        .with_paging(has_more, "Reading sessions...");
    Ok((picker, PageLoader::new(backend.clone(), page_size, events)))
}

/// Fetches picker pages on the runtime and sends each to the loop
/// that drives the view, which applies it with [`apply_event`].
pub(crate) struct PageLoader {
    backend: Backend,
    page_size: usize,
    events: UnboundedSender<ViewEvent>,
}

/// What the page loader sends to the loop that drives the view.
pub(crate) enum ViewEvent {
    /// A page the loader fetched
    Page(io::Result<Page>),
    /// The picker's loading dialog delay has passed since a fetch
    /// began; the redraw shows the dialog if the page is still out
    LoadingDue,
}

/// One page of picker rows.
pub(crate) struct Page {
    items: Vec<PickItem>,
    /// Rows beyond this page exist
    has_more: bool,
}

impl PageLoader {
    fn new(backend: Backend, page_size: usize, events: UnboundedSender<ViewEvent>) -> Self {
        Self {
            backend,
            page_size,
            events,
        }
    }

    /// Fetch the page starting at `offset`, and wake the loop once
    /// the loading dialog is due.
    fn start(&self, offset: usize) {
        let backend = self.backend.clone();
        let page_size = self.page_size;
        let events = self.events.clone();
        tokio::spawn(async move {
            let page = backend
                .list_recent(page_size, offset)
                .await
                .map(|items| Page {
                    has_more: items.len() == page_size,
                    items: pick_items(&backend, items),
                })
                .map_err(|e| io::Error::other(e.to_string()));
            // A closed channel means the loop that would show the
            // page has exited.
            if let Ok(()) = events.send(ViewEvent::Page(page)) {}
        });
        let events = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(picker::LOADING_DIALOG_DELAY).await;
            if let Ok(()) = events.send(ViewEvent::LoadingDue) {}
        });
    }
}

/// Apply a page loader event to the view. A page that lands after the
/// picker closed has nothing to update; a page that failed is the
/// loop's error.
pub(crate) fn apply_event(state: &mut AppState, event: ViewEvent) -> io::Result<()> {
    match event {
        ViewEvent::Page(page) => {
            let page = page?;
            if let Dialog::OpenSession { picker, .. } = &mut state.dialog {
                picker.append(page.items, page.has_more);
            }
        }
        ViewEvent::LoadingDue => {}
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
    events: &UnboundedSender<ViewEvent>,
) -> io::Result<KeyOutcome> {
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
        Dialog::ConfirmShowAll { .. } => {
            handle_confirm_show_all(state, key.code);
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
                KeyCode::Char('s') => state.toggle_source(),
                KeyCode::Char('n') => state.toggle_turns(),
                KeyCode::Char('c') => state.toggle_changes(),
                KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => {
                    state.dialog = Dialog::None;
                }
                _ => {}
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::NotesUnavailable(_) => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc) {
                state.dialog = Dialog::None;
            }
            return Ok(KeyOutcome::Consumed);
        }
        Dialog::GotoEntry { input, cursor } => {
            match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    input.insert(*cursor, c);
                    *cursor += 1;
                }
                KeyCode::Backspace if *cursor > 0 => {
                    *cursor -= 1;
                    input.remove(*cursor);
                }
                KeyCode::Left => *cursor = cursor.saturating_sub(1),
                KeyCode::Right => *cursor = (*cursor + 1).min(input.len()),
                KeyCode::Char('g') | KeyCode::Enter => {
                    let target = input.parse::<usize>().ok();
                    state.dialog = Dialog::None;
                    if let Some(n) = target.filter(|n| *n > 0) {
                        state.goto_entry(n - 1);
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => state.dialog = Dialog::None,
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
                state.dialog = Dialog::NotesUnavailable(state.notes_notice);
            }
            KeyOutcome::Consumed
        }
        KeyCode::Char('v') => {
            state.dialog = Dialog::Options;
            KeyOutcome::Consumed
        }
        KeyCode::Char('o') => {
            let (picker, loader) =
                open_session_picker(backend, Some(&state.doc.session.id), events.clone())?;
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
        KeyCode::Char(c) if c.is_ascii_digit() => {
            state.dialog = Dialog::GotoEntry {
                input: c.to_string(),
                cursor: 1,
            };
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
    expansion: Expansion,
    view: ViewOptions,
}

impl SavedUi {
    /// The view options in effect when the snapshot was taken. A host
    /// rebuilding the view passes these to [`AppState::new`] so the
    /// outline is filtered as the user left it.
    pub(crate) fn view(&self) -> &ViewOptions {
        &self.view
    }
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
    /// Offer to show every entry when the view opened at a session
    /// line whose entry the membership options hide. `y` turns "All
    /// entries" on and selects the entry; anything else keeps the view.
    ConfirmShowAll {
        line: u32,
        entry_index: usize,
    },
    /// Session-open picker (`o`) and the loader for its later pages
    OpenSession {
        picker: Picker,
        loader: PageLoader,
    },
    /// View-options toggles (`v`)
    Options,
    /// Notice that `n` has nothing to write to over a native source
    /// Why a note cannot be added here, shown as a notice
    NotesUnavailable(&'static str),
    /// Go-to-entry field (`0`..`9`): the typed digits and the cursor
    /// offset within them
    GotoEntry {
        input: String,
        cursor: usize,
    },
}

const NOTES_UNAVAILABLE: &str = "Notes cannot be added to native sessions. \
Add the session to the store to write notes for it.";

const PENDING_NOTES_UNAVAILABLE: &str = "Notes cannot be added to pending publish sessions.";

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
    dialog: Dialog,
    view: ViewOptions,
    source: DocSource,
    /// Whether the backend writes notes for this session; gates the
    /// note keys
    notes_writable: bool,
    /// The notice the note key shows when notes are not writable
    notes_notice: &'static str,
    /// Whether the terminal runs the Kitty keyboard protocol; selects
    /// the newline shortcut the editor hint names
    enhanced_keys: bool,
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
            attachment_projection(&doc),
            session_note_ids,
            entry_note_ids,
            entry_hidden(&doc, options),
            dropped_anchors(&doc),
            options.changes,
        );
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        // A pending publish is not in the store, so no backend writes
        // notes for it
        let (notes_writable, notes_notice) = if doc.session.pending {
            (false, PENDING_NOTES_UNAVAILABLE)
        } else {
            (notes_writable, NOTES_UNAVAILABLE)
        };
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
            dialog: Dialog::None,
            view: options.clone(),
            source,
            notes_writable,
            notes_notice,
            enhanced_keys: false,
        }
    }

    /// The view options in effect.
    #[cfg(test)]
    pub(crate) fn view(&self) -> &ViewOptions {
        &self.view
    }

    pub(crate) fn save_ui(&self) -> SavedUi {
        SavedUi {
            selected: self
                .list_state
                .selected()
                .and_then(|i| self.outline.row(i))
                .map(|r| r.kind.clone()),
            offset: self.list_state.offset(),
            body_scroll: self.body_scroll,
            focus_body: self.focus == Focus::Body,
            expansion: self.outline.expansion(),
            view: self.view.clone(),
        }
    }

    /// Re-apply a [`SavedUi`]: seed the outline's expansion state, then
    /// find the saved row by identity in the rebuilt rows. When the row
    /// is gone (e.g. its note was deleted between views), selection
    /// falls back to the top and the saved body position is dropped.
    pub(crate) fn restore_ui(&mut self, saved: &SavedUi) {
        self.outline.set_expansion(saved.expansion.clone());
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
    /// before the first entry leaves the selection where it is. A line
    /// whose entry the membership options hide opens the dialog that
    /// offers to show all entries.
    pub(crate) fn select_line(&mut self, line: u32) {
        let Some(entry_index) = self.doc.entry_index_for_line(line) else {
            return;
        };
        if !self.select_entry(entry_index) {
            self.dialog = Dialog::ConfirmShowAll { line, entry_index };
        }
    }

    /// Select entry `index`'s outline row and scroll it into view, for
    /// hosts opening the view at a specific entry. Returns whether the
    /// entry has a row; a hidden entry leaves the selection unchanged.
    pub(crate) fn select_entry(&mut self, index: usize) -> bool {
        let Some(row) = self
            .outline
            .rows()
            .iter()
            .position(|r| matches!(r.kind, RowKind::Entry { index: i } if i == index))
        else {
            return false;
        };
        self.list_state.select(Some(row));
        self.body_scroll = 0;
        self.center_selected();
        true
    }

    /// Select entry `index`'s outline row, or the nearest visible entry
    /// row before it when that entry is filtered out. Only entry rows
    /// are selected, never a note row. Does nothing when no entry at or
    /// before `index` is visible.
    fn goto_entry(&mut self, index: usize) {
        let row = self.outline.rows().iter().rposition(|r| match r.kind {
            RowKind::Entry { index: i } => i <= index,
            _ => false,
        });
        if let Some(row) = row {
            self.list_state.select(Some(row));
            self.body_scroll = 0;
            self.center_selected();
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
        let attachments = attachment_projection(&doc);
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let hidden = entry_hidden(&doc, &self.view);
        let anchors = dropped_anchors(&doc);
        self.doc = doc;
        let pane_row = self.selected_pane_row();
        self.outline.reload(
            attachments,
            session_note_ids,
            entry_note_ids,
            hidden,
            anchors,
        );
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

    fn show_all_entries(&mut self) {
        self.view.all_entries = true;
        self.apply_filter();
    }

    /// Recompute the membership mask and reapply it, keeping the selection
    /// and its place in the pane when its row survives the rebuild.
    fn apply_filter(&mut self) {
        let (prior, pane_row) = self.selection_snapshot();
        self.outline.set_hidden(entry_hidden(&self.doc, &self.view));
        self.reselect(prior, pane_row);
    }

    /// Toggle the changes of a published session: the dropped-line
    /// rows in the outline, the mark on each redacted entry, and the
    /// records in the content pane.
    fn toggle_changes(&mut self) {
        self.view.changes = !self.view.changes;
        let (prior, pane_row) = self.selection_snapshot();
        self.outline.set_dropped_shown(self.view.changes);
        self.reselect(prior, pane_row);
    }

    /// The selected row's kind and its distance from the top of the
    /// pane, taken before an outline rebuild.
    fn selection_snapshot(&self) -> (Option<RowKind>, usize) {
        let prior = self
            .list_state
            .selected()
            .and_then(|i| self.outline.row(i))
            .map(|r| r.kind.clone());
        (prior, self.selected_pane_row())
    }

    /// Reselect `prior` after a rebuild at the same place in the pane.
    /// A filtered-out entry falls back to the nearest preceding visible
    /// entry; a hidden dropped run falls back to the entry it preceded.
    fn reselect(&mut self, prior: Option<RowKind>, pane_row: usize) {
        let new_sel = prior
            .and_then(|kind| {
                self.outline
                    .rows()
                    .iter()
                    .position(|r| same_row(&r.kind, &kind))
                    .or_else(|| match kind {
                        RowKind::Entry { index } => nearest_prev_entry(&self.outline, index),
                        RowKind::Dropped { index } => {
                            let anchor = self.doc.dropped().get(index).map(|d| d.anchor)?;
                            nearest_prev_entry(&self.outline, anchor)
                        }
                        _ => None,
                    })
            })
            .or(Some(0));
        self.select_at_pane_row(new_sel, pane_row);
    }

    /// Toggle the native source in the content pane. Independent of the
    /// tree filter.
    fn toggle_source(&mut self) {
        self.view.source = !self.view.source;
    }

    fn toggle_turns(&mut self) {
        self.view.turns = !self.view.turns;
    }

    /// Swap in a different session's document, resetting the UI to the
    /// top of the new session.
    fn replace_doc(&mut self, doc: Document) {
        let attachments = attachment_projection(&doc);
        let (session_note_ids, entry_note_ids) = note_projection(&doc);
        let hidden = entry_hidden(&doc, &self.view);
        let anchors = dropped_anchors(&doc);
        self.doc = doc;
        self.outline.reload(
            attachments,
            session_note_ids,
            entry_note_ids,
            hidden,
            anchors,
        );
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
            RowKind::Session
            | RowKind::Attachment { .. }
            | RowKind::AttachmentFile { .. }
            | RowKind::Dropped { .. } => None,
        }
    }

    /// Whether the selected row is one no note targets: a run of
    /// dropped lines, an attachment, or an attachment file.
    fn selected_takes_no_note(&self) -> bool {
        self.list_state
            .selected()
            .and_then(|i| self.outline.row(i))
            .is_some_and(|r| {
                matches!(
                    r.kind,
                    RowKind::Dropped { .. }
                        | RowKind::Attachment { .. }
                        | RowKind::AttachmentFile { .. }
                )
            })
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
        if self.list_state.selected().is_none() || self.selected_takes_no_note() {
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
        (RowKind::Attachment { attachment_id: a }, RowKind::Attachment { attachment_id: b }) => {
            a == b
        }
        (
            RowKind::AttachmentFile {
                attachment_id: a,
                path: a_path,
            },
            RowKind::AttachmentFile {
                attachment_id: b,
                path: b_path,
            },
        ) => a == b && a_path == b_path,
        (RowKind::Entry { index: a }, RowKind::Entry { index: b }) => a == b,
        (RowKind::Note { note_id: a, .. }, RowKind::Note { note_id: b, .. }) => a == b,
        (RowKind::Dropped { index: a }, RowKind::Dropped { index: b }) => a == b,
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
/// bookkeeping types) stays hidden. An invalid entry is always shown:
/// it marks a native line the codec could not decode.
fn entry_visible(label: &str, view: &ViewOptions) -> bool {
    if view.all_entries || label == INVALID_ENTRY_TYPE {
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

/// Outline projection of the document's attachments: each id with its
/// file paths.
fn attachment_projection(doc: &Document) -> Vec<AttachmentFiles> {
    doc.attachments
        .iter()
        .map(|a| AttachmentFiles {
            id: a.id.clone(),
            paths: a.files.iter().map(|f| f.path.clone()).collect(),
        })
        .collect()
}

/// The entry index each run of dropped lines precedes, for the
/// outline to place the run's row.
fn dropped_anchors(doc: &Document) -> Vec<usize> {
    doc.dropped().iter().map(|d| d.anchor).collect()
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

fn handle_confirm_show_all(state: &mut AppState, code: KeyCode) {
    let Dialog::ConfirmShowAll { entry_index, .. } =
        std::mem::replace(&mut state.dialog, Dialog::None)
    else {
        return;
    };
    if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
        state.show_all_entries();
        state.select_entry(entry_index);
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
    draw_dialog(frame, &mut state.dialog, &state.view, state.enhanced_keys);
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
        .map(|(i, row)| row_to_item(row, &state.doc, Some(i) == selected, &state.view))
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
    view: &ViewOptions,
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
        RowKind::Attachment { .. } => Line::from(vec![prefix, Span::raw("attachment")]),
        RowKind::AttachmentFile { path, .. } => Line::from(vec![prefix, Span::raw(path.clone())]),
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
            let turn = view
                .turns
                .then(|| doc.turns.get(*index).copied().flatten())
                .flatten();
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
            if view.changes && !doc.entry_redactions(*index).is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled("redacted", styles::Text::dim()));
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
        RowKind::Dropped { index } => Line::from(vec![
            prefix,
            Span::styled(dropped_header(doc, *index), styles::Text::aside()),
        ]),
    };
    ListItem::new(line)
}

/// The label of a run of dropped lines: how many, `✂ --- 2 lines ---`,
/// or `✂ --- 1 line ---`. The original line numbers are in the body.
fn dropped_header(doc: &Document, index: usize) -> String {
    match doc.dropped().get(index) {
        Some(d) => match d.line_end - d.line + 1 {
            1 => "✂ --- 1 line ---".to_string(),
            n => format!("✂ --- {n} lines ---"),
        },
        None => "✂ --- ? lines ---".to_string(),
    }
}

fn line_range(line: u32, line_end: u32) -> String {
    if line_end > line {
        format!("{line}-{line_end}")
    } else {
        line.to_string()
    }
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
            let lines = session_lines(&state.doc.session.attrs, padded_width(inner));
            let source = state
                .view
                .source
                .then(|| json::render(&state.doc.session.value, inner.width));
            draw_session(frame, state, lines, source, inner);
        }
        Some(RowKind::Entry { index }) => {
            let redactions = if state.view.changes {
                redaction_lines(&state.doc.entry_redactions(index))
            } else {
                Vec::new()
            };
            let snap = state.doc.entries.get(index).map(|e| EntrySnap {
                source: state
                    .view
                    .source
                    .then(|| json::render(&e.value, inner.width)),
                body: state.doc.entry_body(index),
                redactions,
            });
            match snap {
                Some(snap) => draw_entry(frame, state, &snap, inner),
                None => draw_placeholder(frame, "(missing entry)", inner),
            }
        }
        Some(RowKind::Attachment { attachment_id }) => match state.doc.attachment(&attachment_id) {
            Some(a) => {
                let lines = session_attachment::body(a, padded_width(inner));
                draw_lines(frame, state, lines, inner);
            }
            None => draw_placeholder(frame, "(missing attachment)", inner),
        },
        Some(RowKind::AttachmentFile {
            attachment_id,
            path,
        }) => {
            match state
                .doc
                .attachment(&attachment_id)
                .and_then(|a| a.file(&path))
            {
                Some(file) => {
                    let lines = session_attachment::file_body(file);
                    draw_lines(frame, state, lines, inner);
                }
                None => draw_placeholder(frame, "(missing file)", inner),
            }
        }
        Some(RowKind::Note { note_id, .. }) => match state.doc.note(&note_id) {
            Some(note) => draw_note(frame, note, inner),
            None => draw_placeholder(frame, "(missing note)", inner),
        },
        Some(RowKind::Dropped { index }) => match state.doc.dropped().get(index) {
            Some(dropped) => draw_dropped(frame, &state.doc, dropped, inner),
            None => draw_placeholder(frame, "(missing removed run)", inner),
        },
        None => draw_placeholder(frame, "(no selection)", inner),
    }

    draw_body_scrollbar(frame, state, active, area);
}

fn body_title(doc: &Document, kind: Option<&RowKind>) -> String {
    match kind {
        Some(RowKind::Session) => doc.session.id.clone(),
        Some(RowKind::Attachment { .. }) => format!("{} • attachment", doc.session.id),
        Some(RowKind::AttachmentFile { path, .. }) => format!("{} • {path}", doc.session.id),
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
        Some(RowKind::Dropped { index }) => dropped_header(doc, *index),
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

/// The content of a run of dropped lines: the `line` records that
/// cover it, one per line with its reason and author.
fn draw_dropped(frame: &mut Frame, doc: &Document, dropped: &Dropped, area: Rect) {
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "Lines {} of the original were removed",
                line_range(dropped.line, dropped.line_end)
            ),
            styles::Text::dim(),
        )),
        Line::from(""),
    ];
    lines.extend(redaction_lines(&doc.dropped_redactions(dropped)));
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().padding(Padding::uniform(1)));
    frame.render_widget(p, area);
}

/// The records as the content pane lists them, one per line in three
/// left-aligned columns: where it applied, its reason when it has one,
/// and its author. A text record names its pointer and its byte
/// offsets in the published value, a value record its selector, a
/// line record its line range, an attr record its attribute, and a
/// title record the line the publish added. The columns are sized to
/// the records given, so each list aligns on its own.
fn redaction_lines(records: &[&Redaction]) -> Vec<Line<'static>> {
    let rows: Vec<(String, &str, &str)> = records
        .iter()
        .map(|r| {
            (
                redaction_location(r),
                r.reason.as_deref().unwrap_or_default(),
                r.author.as_str(),
            )
        })
        .collect();
    let width = |col: fn(&(String, &str, &str)) -> usize| rows.iter().map(col).max().unwrap_or(0);
    let location_width = width(|row| row.0.chars().count());
    let reason_width = width(|row| row.1.chars().count());
    rows.into_iter()
        .map(|(location, reason, author)| {
            Line::from(vec![
                Span::raw(format!("{location:<location_width$}  ")),
                Span::raw(format!("{reason:<reason_width$}  ")),
                Span::styled(author.to_string(), styles::Text::dim()),
            ])
        })
        .collect()
}

/// Where a record applied, as its row's first column.
fn redaction_location(r: &Redaction) -> String {
    let reference = r.reference.clone().unwrap_or_default();
    match r.kind {
        ChangeKind::Entry => line_range(r.line, r.line_end.unwrap_or(r.line)),
        ChangeKind::Text => match (r.span_start, r.span_end) {
            (Some(start), Some(end)) => format!("{reference} {start}-{end}"),
            _ => reference,
        },
        ChangeKind::Value => format!("values {reference}"),
        ChangeKind::Title => format!("title added at line {}", r.line),
    }
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
    /// The entry's value as rows fitted to the pane, when the source
    /// view option is on
    source: Option<Vec<Line<'static>>>,
    body: Option<EntryBody>,
    /// The entry's redaction records as listed, empty when the view
    /// option is off or the entry has none
    redactions: Vec<Line<'static>>,
}

fn draw_entry(frame: &mut Frame, state: &mut AppState, entry: &EntrySnap, area: Rect) {
    let mut sections: Vec<Section> = Vec::new();
    if let Some(body) = &entry.body {
        let width = padded_width(area);
        let lines = match body {
            EntryBody::Text { text, tags } => message::body(text, tags, width),
            EntryBody::ToolCalls(calls) => tool::calls(calls, width),
            EntryBody::ToolResults(results) => tool::results(results, width),
            EntryBody::Attachment(attachment) => attachment::body(attachment, width),
            EntryBody::InvalidEntry(invalid) => invalid_entry::body(invalid, width),
            EntryBody::Selection(selected) => selection::body(selected, width),
        };
        let panel = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::uniform(1)));
        sections.push(Section::from_paragraph(panel, area.width));
    }
    if !entry.redactions.is_empty() {
        let mut lines = vec![Line::from(Span::styled(
            "--- redacted ---",
            styles::Text::dim(),
        ))];
        lines.extend(entry.redactions.iter().cloned());
        sections.push(Section::from_paragraph(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(Padding::new(1, 1, 0, 1))),
            area.width,
        ));
    }
    if let Some(source) = &entry.source {
        sections.push(source_banner(area.width));
        sections.push(Section::from_rows(source.clone()));
    }

    draw_stack(frame, state, sections, area);
}

/// The columns a body section has for text inside its one column of
/// padding on each side.
fn padded_width(area: Rect) -> u16 {
    area.width.saturating_sub(2)
}

fn source_banner(width: u16) -> Section {
    Section::from_paragraph(
        Paragraph::new(Line::from(Span::styled(
            "--- source ---",
            styles::Text::dim(),
        ))),
        width,
    )
}

/// The session's attributes as the root row shows them: the title in
/// bold, a blank line, then the fields as caption/value rows.
fn session_lines(attrs: &SessionAttrs, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !attrs.title.is_empty() {
        lines.push(Line::from(Span::styled(
            attrs.title.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }
    let fields: Vec<(&'static str, &str)> = attrs
        .fields
        .iter()
        .map(|(label, value)| (*label, value.as_str()))
        .collect();
    lines.extend(attr_lines(&fields, width));
    lines
}

/// Lines as one wrapped, scrollable section.
fn draw_lines(frame: &mut Frame, state: &mut AppState, lines: Vec<Line<'static>>, area: Rect) {
    draw_session(frame, state, lines, None, area);
}

/// The root row: the session's attributes, then its row as a source
/// banner and fitted rows when the source view option is on.
fn draw_session(
    frame: &mut Frame,
    state: &mut AppState,
    lines: Vec<Line<'static>>,
    source: Option<Vec<Line<'static>>>,
    area: Rect,
) {
    let mut sections = vec![Section::from_paragraph(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::uniform(1))),
        area.width,
    )];
    if let Some(source) = source {
        sections.push(source_banner(area.width));
        sections.push(Section::from_rows(source));
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

    /// Rows already fitted to the pane, painted without wrapping.
    fn from_rows(rows: Vec<Line<'static>>) -> Self {
        let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        Self {
            widget: Box::new(move |area, buf| {
                ratatui::widgets::Widget::render(Paragraph::new(rows), area, buf);
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

fn draw_dialog(frame: &mut Frame, dialog: &mut Dialog, view: &ViewOptions, enhanced_keys: bool) {
    match dialog {
        Dialog::None => {}
        Dialog::Options => draw_options(frame, view),
        Dialog::NotesUnavailable(notice) => {
            crate::dialog::draw_wrapped_message(frame, notice, "Enter dismiss")
        }
        Dialog::NoteType { .. } => draw_note_type(frame),
        Dialog::AddNote { editor, title, .. } => draw_editor(frame, title, editor, enhanced_keys),
        Dialog::EditNote { editor, title, .. } => draw_editor(frame, title, editor, enhanced_keys),
        Dialog::ConfirmCancel { pending } => {
            match pending.as_mut() {
                Dialog::AddNote { editor, title, .. } => {
                    draw_editor(frame, title, editor, enhanced_keys)
                }
                Dialog::EditNote { editor, title, .. } => {
                    draw_editor(frame, title, editor, enhanced_keys)
                }
                _ => {}
            }
            draw_confirm(frame, "Cancel your changes?");
        }
        Dialog::ConfirmDelete { .. } => {
            draw_confirm(frame, "Delete this note? This cannot be undone.")
        }
        Dialog::ConfirmShowAll { line, .. } => draw_confirm(
            frame,
            &format!("Line {line} is not visible. Show all entries?"),
        ),
        Dialog::OpenSession { picker, .. } => picker.draw(frame),
        Dialog::GotoEntry { input, cursor } => {
            let hint = hint::dialog_line(&[("g", "go"), ("Esc", "cancel")]);
            crate::dialog::draw_input(frame, "Go to entry", input, *cursor, hint)
        }
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
        option_line('s', view.source, false, "Native source"),
        option_line('n', view.turns, false, "Turns"),
        option_line('c', view.changes, false, "Changes"),
    ];
    let hint = hint::dialog_line(&[("Enter", "close")]);
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

fn draw_editor(frame: &mut Frame, title: &str, editor: &mut TextArea, enhanced_keys: bool) {
    let hint = format!(
        "Enter save · {} newline · Esc cancel",
        hint::newline_keys(enhanced_keys)
    );
    crate::dialog::draw_text_area(frame, title, None, editor, &hint);
}

fn draw_confirm(frame: &mut Frame, message: &str) {
    crate::dialog::draw_wrapped_message(frame, message, "y / n");
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
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEvent;

    use super::*;
    use crate::doc::{AttachmentFile, Entry, Session, SessionAttachment, ToolUse};
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
        let (events, _view_events) = unbounded_channel();
        handle_key(
            state,
            KeyEvent::new(code, KeyModifiers::NONE),
            backend,
            &events,
        )
        .unwrap()
    }

    /// Modifiers of the first `ch` in the first row that contains
    /// `marker`, for asserting a cell's style.
    fn cell_modifiers(terminal: &Terminal<TestBackend>, marker: &str, ch: char) -> Modifier {
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
                    return buffer[(x, y)].modifier;
                }
            }
        }
        panic!("no row contains {marker:?}");
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
                RowKind::Attachment { attachment_id } => format!("attachment {attachment_id}"),
                RowKind::AttachmentFile { path, .. } => format!("file {path}"),
                RowKind::Entry { index } => format!("entry {index}"),
                RowKind::Note { note_id, .. } => {
                    format!("note {}", state.doc.note(note_id).unwrap().text())
                }
                RowKind::Dropped { index } => format!("dropped {index}"),
            })
            .collect()
    }

    /// Adding, editing, and deleting a note through the keys writes
    /// the store and keeps the outline in step with it.
    #[test]
    fn save_ui_carries_the_view_options() {
        let doc = Document {
            session: Session {
                id: "s1".into(),
                value: serde_json::json!({}),
                pending: false,
                attrs: SessionAttrs::default(),
            },
            entries: Vec::new(),
            turns: Vec::new(),
            tool_uses: Vec::new(),
            notes: Vec::new(),
            attachments: Vec::new(),
            redactions: Vec::new(),
        };
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        state.toggle_tools();
        state.toggle_turns();
        state.toggle_changes();
        let saved = state.save_ui();
        assert!(saved.view().tools);
        assert!(saved.view().turns);
        assert!(saved.view().changes);
        assert!(saved.view().user);
    }

    /// Record lines are three left-aligned columns sized to the list:
    /// where the change applied, its reason, and its author. A title
    /// record leaves its reason column blank.
    #[test]
    fn redaction_lines_align_the_columns_of_the_list() {
        use crate::doc::Redaction;
        let text = |records: &[&Redaction]| -> Vec<String> {
            redaction_lines(records)
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
                .collect()
        };
        let title = Redaction {
            kind: ChangeKind::Title,
            line: 6,
            line_end: None,
            reference: None,
            span_start: None,
            span_end: None,
            reason: None,
            author: "user:t".into(),
        };
        let value = Redaction {
            kind: ChangeKind::Value,
            line: 0,
            line_end: None,
            reference: Some("$.cwd".into()),
            span_start: None,
            span_end: None,
            reason: Some("system".into()),
            author: "filter:system".into(),
        };
        assert_eq!(
            text(&[&title, &value]),
            [
                "title added at line 6          user:t",
                "values $.cwd           system  filter:system",
            ]
        );
        assert_eq!(text(&[&value]), ["values $.cwd  system  filter:system"]);
    }

    /// A run's label counts its lines, singular for one.
    #[test]
    fn dropped_header_counts_the_lines_of_the_run() {
        let mut doc = doc_with_redactions();
        assert_eq!(dropped_header(&doc, 0), "✂ --- 2 lines ---");
        doc.redactions[1].line_end = Some(2);
        assert_eq!(dropped_header(&doc, 0), "✂ --- 1 line ---");
        assert_eq!(dropped_header(&doc, 1), "✂ --- ? lines ---");
    }

    /// A derived session: five kept entries of an original whose
    /// lines 2-3 were dropped, with a span redacted on the first entry.
    fn doc_with_redactions() -> Document {
        use crate::doc::Redaction;
        let mut doc = doc_with_entries(5);
        doc.entries[0].message_text = Some("hello [REDACTED]".into());
        doc.redactions = vec![
            Redaction {
                kind: ChangeKind::Text,
                line: 1,
                line_end: None,
                reference: Some("/message/content".into()),
                span_start: Some(6),
                span_end: Some(16),
                reason: Some("pii".into()),
                author: "filter:pii".into(),
            },
            Redaction {
                kind: ChangeKind::Entry,
                line: 2,
                line_end: Some(3),
                reference: None,
                span_start: None,
                span_end: None,
                reason: Some("scope".into()),
                author: "user:t".into(),
            },
        ];
        doc
    }

    /// The `c` option adds a row for each dropped run before the entry
    /// that followed it, marks the entries with text redactions, and
    /// lists the records in the content pane; off again, the outline
    /// is as before and the selection falls back to the following
    /// entry.
    #[tokio::test(flavor = "multi_thread")]
    async fn changes_option_shows_dropped_runs_and_records() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_redactions(),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        assert_eq!(
            outline_labels(&state),
            [
                "<Session>",
                "entry 0",
                "entry 1",
                "entry 2",
                "entry 3",
                "entry 4"
            ]
        );
        press(&mut state, &backend, KeyCode::Char('j'));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(!text.contains("redacted"), "{text}");

        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('c'));
        assert!(state.view.changes);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert!(screen(&terminal).contains("c [x] Changes"));
        press(&mut state, &backend, KeyCode::Enter);
        assert_eq!(
            outline_labels(&state),
            [
                "<Session>",
                "entry 0",
                "dropped 0",
                "entry 1",
                "entry 2",
                "entry 3",
                "entry 4"
            ]
        );
        // Entry 0 keeps the selection and shows its span record
        assert_eq!(state.selected_entry_index(), Some(0));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("1 user redacted"), "{text}");
        assert!(text.contains("--- redacted ---"), "{text}");
        assert!(
            text.contains("/message/content 6-16  pii  filter:pii"),
            "{text}"
        );
        assert!(text.contains("✂ --- 2 lines ---"), "{text}");

        // With the native source on, a blank line separates the
        // records from the source header
        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('s'));
        press(&mut state, &backend, KeyCode::Enter);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        let rows: Vec<&str> = text.lines().collect();
        let record = rows
            .iter()
            .position(|r| r.contains("filter:pii"))
            .expect("the span record row should render");
        // The body pane is the last bordered column of a row
        let body = |row: &str| row.rsplit('│').nth(1).unwrap_or("").trim().to_string();
        assert!(body(rows[record + 1]).is_empty(), "{text}");
        assert_eq!(body(rows[record + 2]), "--- source ---", "{text}");
        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('s'));
        press(&mut state, &backend, KeyCode::Enter);

        // The dropped row lists the line records that cover it, and
        // `n` offers no note on it
        press(&mut state, &backend, KeyCode::Char('j'));
        assert!(matches!(
            selected_kind(&state),
            RowKind::Dropped { index: 0 }
        ));
        assert_eq!(state.selected_entry_index(), None);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("Lines 2-3 of the original were removed"),
            "{text}"
        );
        assert!(text.contains("2-3  scope  user:t"), "{text}");
        press(&mut state, &backend, KeyCode::Char('n'));
        assert!(matches!(state.dialog, Dialog::NotesUnavailable(_)));
        press(&mut state, &backend, KeyCode::Enter);

        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('c'));
        press(&mut state, &backend, KeyCode::Enter);
        assert!(!state.view.changes);
        assert_eq!(
            outline_labels(&state),
            [
                "<Session>",
                "entry 0",
                "entry 1",
                "entry 2",
                "entry 3",
                "entry 4"
            ]
        );
        assert_eq!(state.selected_entry_index(), Some(1));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        assert!(!screen(&terminal).contains("redacted"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn select_line_opens_at_the_entry_for_a_session_line() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        let doc = backend.load(&f.session_id).await.unwrap();
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 1"]);
        assert_eq!(state.selected_entry_index(), None);

        // The fixture's entries sit at lines 1 and 2.
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
                pending: false,
                attrs: SessionAttrs::default(),
            },
            entries: (1..=count)
                .map(|line| Entry {
                    line,
                    uuid: Some(format!("u{line}")),
                    entry_type: "user".into(),
                    message_subtype: Some("text".into()),
                    message_text: None,
                    ide_tags: None,
                    value: serde_json::json!({"type": "user"}),
                })
                .collect(),
            turns: Vec::new(),
            tool_uses: Vec::new(),
            notes: Vec::new(),
            attachments: Vec::new(),
            redactions: Vec::new(),
        }
    }

    /// A message entry's body is its `message.text`, rendered as
    /// markdown; text that is not markdown renders as it is. A
    /// thinking row renders the same way as a prompt or a reply.
    #[test]
    fn message_rows_render_their_text_column() {
        let mut doc = doc_with_entries(3);
        doc.entries[0].message_text = Some("Hello **world** from the prompt".into());
        doc.entries[1].entry_type = "assistant".into();
        doc.entries[1].message_subtype = Some("thinking".into());
        doc.entries[1].message_text = Some("- step one\n- step two".into());
        doc.entries[2].entry_type = "assistant".into();
        doc.entries[2].message_text = Some("a < b > c {not markdown".into());
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        for (line, expected) in [
            (1, "Hello world from the prompt"),
            (2, "step one"),
            (3, "a < b > c {not markdown"),
        ] {
            state.select_line(line);
            terminal.draw(|frame| draw(frame, &mut state)).unwrap();
            let text = screen(&terminal);
            assert!(text.contains(expected), "line {line}:\n{text}");
            assert!(!text.contains("--- source ---"), "line {line}:\n{text}");
        }
    }

    /// A session attachment is a child of the session row labelled
    /// `attachment`, before the session's notes. Its body is its
    /// attributes as caption/value rows, then its files with their
    /// sizes in a column. It opens to a row per file whose body is the
    /// file's text, or a placeholder when the bytes are not UTF-8. The
    /// note key opens no dialog on either row.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_attachment_rows_render_their_attributes_and_files() {
        let f = fixture();
        let backend = Backend::stored(f.store).await;
        backend
            .add_note(&f.session_id, None, "comment", "whole".into(), "user:t")
            .unwrap();
        let mut doc = backend.load(&f.session_id).await.unwrap();
        doc.attachments.push(SessionAttachment {
            id: "a1".into(),
            name: Some("cfg".into()),
            key: None,
            targets: vec!["session:s1".into()],
            root: PathBuf::from("/w/cfg"),
            file_count: 2,
            size: 11,
            created_ms: 0,
            modified_ms: 0,
            files: vec![
                AttachmentFile {
                    path: "settings.json".into(),
                    size: 8,
                    text: Some("{\"a\": 1}".into()),
                },
                AttachmentFile {
                    path: "x.bin".into(),
                    size: 3,
                    text: None,
                },
            ],
        });
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 1"]);
        state.outline.toggle(0);
        assert_eq!(
            outline_labels(&state),
            [
                "<Session>",
                "attachment a1",
                "note whole",
                "entry 0",
                "entry 1"
            ]
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        state.list_state.select(Some(1));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("• attachment"), "{text}");
        assert!(text.contains("Id        a1"), "{text}");
        assert!(text.contains("Name      cfg"), "{text}");
        assert!(!text.contains("Key"), "{text}");
        assert!(text.contains("Targets   session:s1"), "{text}");
        assert!(text.contains("Root      /w/cfg"), "{text}");
        assert!(text.contains("Files     2"), "{text}");
        assert!(text.contains("Size      11 B"), "{text}");
        assert!(text.contains("settings.json  8 B"), "{text}");
        assert!(text.contains("x.bin          3 B"), "{text}");
        press(&mut state, &backend, KeyCode::Char('n'));
        assert!(matches!(state.dialog, Dialog::None));

        state.outline.toggle(1);
        assert_eq!(
            outline_labels(&state),
            [
                "<Session>",
                "attachment a1",
                "file settings.json",
                "file x.bin",
                "note whole",
                "entry 0",
                "entry 1",
            ]
        );
        state.list_state.select(Some(2));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("• settings.json"), "{text}");
        assert!(text.contains("{\"a\": 1}"), "{text}");
        press(&mut state, &backend, KeyCode::Char('n'));
        assert!(matches!(state.dialog, Dialog::None));

        state.list_state.select(Some(3));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("• x.bin"), "{text}");
        assert!(text.contains("(binary file)"), "{text}");
    }

    /// An attachment row's body opens with its type, and its subtype
    /// when it has one, then the text the harness sent to the model
    /// without its wrapper, item by item. A row the harness never sent
    /// shows its type and the attributes its selection rule names.
    #[test]
    fn attachment_rows_render_their_type_and_rendered_text() {
        let mut doc = doc_with_entries(3);
        for entry in &mut doc.entries {
            entry.entry_type = "attachment".into();
            entry.message_subtype = None;
        }
        doc.entries[0].value = serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "environment", "snapshot": {"platform": "linux"}},
            "rendered": [
                {"content": "<system-reminder>\n# Environment\nPlatform: linux\n</system-reminder>"},
                {"content": "second item"},
            ],
        });
        doc.entries[1].value = serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "credential_org", "organizationUuid": "o1"},
        });
        doc.entries[2].value = serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "auto_mode", "reminderType": "bash_first"},
            "rendered": [{"content": "<system-reminder>\nWhile auto mode is active\n</system-reminder>"}],
        });
        let options = ViewOptions {
            all_entries: true,
            ..ViewOptions::default()
        };
        let mut state = AppState::new(doc, &options, DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        state.select_line(1);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Type  environment"), "{text}");
        assert!(text.contains("second item"), "{text}");
        assert!(!text.contains("system-reminder"), "{text}");
        let rows: Vec<&str> = text.lines().collect();
        let header = rows
            .iter()
            .position(|r| r.contains("Type  environment"))
            .unwrap();
        let body = |row: &str| row.rsplit('│').nth(1).unwrap_or("").to_string();
        assert!(body(rows[header + 1]).trim().is_empty(), "{text}");
        assert!(body(rows[header + 2]).contains("# Environment"), "{text}");

        state.select_line(2);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Type              credential_org"), "{text}");
        assert!(text.contains("organizationUuid  o1"), "{text}");
        assert!(!text.contains("Subtype"), "{text}");

        state.select_line(3);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Type     auto_mode"), "{text}");
        assert!(text.contains("Subtype  bash_first"), "{text}");
        assert!(text.contains("While auto mode is active"), "{text}");
    }

    /// The root row shows the session's attributes as the listing
    /// does: the title in bold, then one line per field, padded like
    /// an entry body. The row's JSON follows a source banner only when
    /// the source option is on.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_root_row_shows_the_session_attributes() {
        let mut doc = doc_with_entries(1);
        doc.session.value = serde_json::json!({"origin": "claude:abc"});
        doc.session.attrs = SessionAttrs {
            title: "Fix the widget".into(),
            fields: vec![
                ("Project", "proj".into()),
                ("Type", "claude".into()),
                ("Model", "claude-x".into()),
                ("Size", "1.5 kB".into()),
                ("Messages", "2".into()),
                ("Created", "3h ago".into()),
            ],
        };
        let backend = test_backend().await;
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        state.select_first();
        assert!(matches!(selected_kind(&state), RowKind::Session));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        let rows: Vec<&str> = text.lines().collect();
        // The body pane is the last bordered column of a row
        let body = |row: &str| row.rsplit('│').nth(1).unwrap_or("").to_string();
        let title = rows
            .iter()
            .position(|r| r.contains("Fix the widget"))
            .unwrap_or_else(|| panic!("{text}"));
        // One padding row above and one column to the left
        assert!(body(rows[title - 1]).trim().is_empty(), "{text}");
        assert!(body(rows[title]).starts_with(" Fix the widget"), "{text}");
        assert!(
            cell_modifiers(&terminal, "Fix the widget", 'F').contains(Modifier::BOLD),
            "{text}"
        );
        assert!(body(rows[title + 1]).trim().is_empty(), "{text}");
        // The labels are padded to the widest plus two spaces so the
        // values share a column, and they take the caption style
        let expected = [
            " Project   proj",
            " Type      claude",
            " Model     claude-x",
            " Size      1.5 kB",
            " Messages  2",
            " Created   3h ago",
        ];
        for (i, line) in expected.iter().enumerate() {
            assert!(body(rows[title + 2 + i]).starts_with(line), "{text}");
        }
        assert!(
            cell_modifiers(&terminal, "Project ", 'P').contains(Modifier::DIM),
            "{text}"
        );
        assert!(
            !cell_modifiers(&terminal, "Project ", 'p').contains(Modifier::DIM),
            "{text}"
        );
        assert!(!text.contains("--- source ---"), "{text}");
        assert!(!text.contains("claude:abc"), "{text}");

        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('s'));
        press(&mut state, &backend, KeyCode::Enter);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        let rows: Vec<&str> = text.lines().collect();
        let banner = rows
            .iter()
            .position(|r| r.contains("--- source ---"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(banner > title + 7, "{text}");
        assert!(
            rows[banner + 1..]
                .iter()
                .any(|r| r.contains("\"origin\": \"claude:abc\"")),
            "{text}"
        );
    }

    /// An entry with no rendering shows nothing until the source option
    /// is on; then it shows the same header and YAML every entry does.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_without_a_body_is_blank_until_the_source_is_shown() {
        let mut doc = doc_with_entries(1);
        doc.entries[0].entry_type = "system".into();
        doc.entries[0].message_subtype = None;
        doc.entries[0].value =
            serde_json::json!({"type": "system", "subtype": "stop_hook_summary"});
        let backend = test_backend().await;
        let view = ViewOptions {
            all_entries: true,
            ..ViewOptions::default()
        };
        let mut state = AppState::new(doc, &view, DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        state.select_line(1);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(!text.contains("--- source ---"), "{text}");
        assert!(!text.contains("stop_hook_summary"), "{text}");

        press(&mut state, &backend, KeyCode::Char('v'));
        press(&mut state, &backend, KeyCode::Char('s'));
        press(&mut state, &backend, KeyCode::Enter);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        let rows: Vec<&str> = text.lines().collect();
        let header = rows
            .iter()
            .position(|r| r.contains("--- source ---"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            rows[header + 1..]
                .iter()
                .any(|r| r.contains("\"subtype\": \"stop_hook_summary\"")),
            "{text}"
        );
    }

    /// A tool call row shows each call's tool and input from the
    /// `tool_use` table; a tool result row shows each result's tool,
    /// error status, and output. Neither reads the message text.
    #[test]
    fn tool_rows_render_from_the_tool_use_table() {
        let mut doc = doc_with_entries(2);
        doc.entries[0].entry_type = "assistant".into();
        doc.entries[0].message_subtype = Some("tool_use".into());
        doc.entries[0].message_text = Some("PROSE-NOT-SHOWN".into());
        doc.entries[1].message_subtype = Some("tool_result".into());
        doc.entries[1].message_text = Some("RESULT-TEXT-NOT-SHOWN".into());
        doc.tool_uses = vec![ToolUse {
            line: 1,
            result_line: Some(2),
            name: "Bash".into(),
            input: r#"{"command":"ls -la"}"#.into(),
            output: Some("total 0\nfile.txt".into()),
            is_error: Some(true),
        }];
        let options = ViewOptions::parse("tools").unwrap();
        let mut state = AppState::new(doc, &options, DocSource::Backend, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        state.select_line(1);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Tool  Bash"), "{text}");
        assert!(text.contains(r#""command": "ls -la""#), "{text}");
        assert!(!text.contains("PROSE-NOT-SHOWN"), "{text}");

        state.select_line(2);
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Tool    Bash"), "{text}");
        assert!(text.contains("Status  error"), "{text}");
        assert!(text.contains("file.txt"), "{text}");
        assert!(!text.contains("RESULT-TEXT-NOT-SHOWN"), "{text}");
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
        // that it is the eighth row of the pane.
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

    /// A document of `count` user entries with the entry at `hidden`
    /// (zero-based) typed as a system entry, which the default options
    /// hide.
    fn doc_with_hidden_entry(count: u32, hidden: usize) -> Document {
        let mut doc = doc_with_entries(count);
        doc.entries[hidden].entry_type = "system".into();
        doc.entries[hidden].value = serde_json::json!({"type": "system"});
        doc
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn select_line_on_a_hidden_entry_offers_to_show_all_entries() {
        let backend = Backend::stored(fixture().store).await;
        let doc = doc_with_hidden_entry(3, 1);
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 2"]);

        // Entry 1 sits at line 2 and is hidden; the dialog opens in
        // place of a selection.
        state.select_line(2);
        assert!(matches!(
            state.dialog,
            Dialog::ConfirmShowAll {
                line: 2,
                entry_index: 1
            }
        ));
        assert_eq!(state.selected_entry_index(), None);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Line 2 is not visible. Show all entries?"));
        assert!(text.contains("y / n"));

        press(&mut state, &backend, KeyCode::Char('y'));
        assert!(matches!(state.dialog, Dialog::None));
        assert!(state.view.all_entries);
        assert_eq!(
            outline_labels(&state),
            ["<Session>", "entry 0", "entry 1", "entry 2"]
        );
        assert_eq!(state.selected_entry_index(), Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declining_to_show_all_entries_keeps_the_view() {
        let backend = Backend::stored(fixture().store).await;
        for code in [KeyCode::Char('n'), KeyCode::Char('q'), KeyCode::Esc] {
            let doc = doc_with_hidden_entry(3, 1);
            let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
            state.select_line(2);
            assert!(matches!(state.dialog, Dialog::ConfirmShowAll { .. }));

            press(&mut state, &backend, code);
            assert!(matches!(state.dialog, Dialog::None));
            assert!(!state.view.all_entries);
            assert_eq!(outline_labels(&state), ["<Session>", "entry 0", "entry 2"]);
            assert_eq!(state.selected_entry_index(), None);
        }
    }

    #[test]
    fn select_line_before_the_first_entry_opens_no_dialog() {
        let doc = doc_with_hidden_entry(3, 0);
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        state.select_line(0);
        assert!(matches!(state.dialog, Dialog::None));
        assert_eq!(state.selected_entry_index(), None);
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
        let backend = Backend::native(&f.spec)
            .cache_root(&f.cache_root)
            .open()
            .await
            .unwrap();
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
        assert!(matches!(state.dialog, Dialog::NotesUnavailable(_)));
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
            source: false,
            turns: false,
            changes: false,
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
        assert!(text.contains("s [ ] Native source"), "{text}");
        assert!(text.contains("Enter close"), "{text}");
        assert!(!text.contains("q/Enter"), "{text}");
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

        // Enter closes the menu, and so does `q`, which the hint does
        // not list.
        press(&mut state, &backend, KeyCode::Enter);
        assert!(matches!(state.dialog, Dialog::None));
        press(&mut state, &backend, KeyCode::Char('v'));
        assert!(matches!(state.dialog, Dialog::Options));
        press(&mut state, &backend, KeyCode::Char('q'));
        assert!(matches!(state.dialog, Dialog::None));
        assert!(state.view.all_entries, "q keeps the toggled options");
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
        state.toggle_source();
        assert!(state.view.source);
        assert!(
            state.view.all_entries,
            "source is independent of membership"
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
        let backend = Backend::native(&f.spec)
            .cache_root(&f.cache_root)
            .open()
            .await
            .unwrap();
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

    /// A pending publish takes no notes even over the store backend:
    /// the note key shows the pending notice, and edit and delete do
    /// nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pending_publish_refuses_notes_with_its_own_notice() {
        let backend = test_backend().await;
        let mut doc = doc_with_entries(1);
        doc.session.pending = true;
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        press(&mut state, &backend, KeyCode::Char('n'));
        assert!(
            matches!(
                state.dialog,
                Dialog::NotesUnavailable(PENDING_NOTES_UNAVAILABLE)
            ),
            "{:?}",
            matches!(state.dialog, Dialog::NotesUnavailable(_))
        );
        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("pending publish sessions"), "{text}");
        press(&mut state, &backend, KeyCode::Enter);
        assert!(matches!(state.dialog, Dialog::None));
        press(&mut state, &backend, KeyCode::Char('e'));
        assert!(matches!(state.dialog, Dialog::None));
    }

    /// A doc-only backend for key tests that never read the store
    async fn test_backend() -> Backend {
        Backend::stored(fixture().store).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_digit_opens_the_goto_entry_dialog_seeded_with_it() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        press(&mut state, &backend, KeyCode::Char('3'));
        match &state.dialog {
            Dialog::GotoEntry { input, cursor } => {
                assert_eq!(input, "3");
                assert_eq!(*cursor, 1);
            }
            _ => panic!("a digit should open the go-to-entry dialog"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_entry_selects_the_entry_whose_number_was_typed() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        // Entry number 3 is entry index 2
        type_text(&mut state, &backend, "3");
        press(&mut state, &backend, KeyCode::Char('g'));
        assert!(matches!(state.dialog, Dialog::None));
        assert_eq!(state.selected_entry_index(), Some(2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_entry_edits_the_field_with_backspace() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        type_text(&mut state, &backend, "19");
        press(&mut state, &backend, KeyCode::Backspace);
        press(&mut state, &backend, KeyCode::Enter);
        // "19" backspaced to "1" selects entry number 1, entry index 0
        assert_eq!(state.selected_entry_index(), Some(0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_a_hidden_entry_selects_the_prior_visible_entry() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_alternating_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        // Hide the assistant entries, the odd indices
        state.toggle_assistant();
        // Entry number 4 is entry index 3, assistant and now hidden; the
        // prior visible entry is index 2
        type_text(&mut state, &backend, "4");
        press(&mut state, &backend, KeyCode::Char('g'));
        assert_eq!(state.selected_entry_index(), Some(2));
    }

    /// The selected row kind, panicking when nothing is selected
    fn selected_kind(state: &AppState) -> RowKind {
        let i = state.list_state.selected().unwrap();
        state.outline.row(i).unwrap().kind.clone()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_a_hidden_entry_lands_on_an_entry_not_a_note() {
        use crate::doc::Note;
        use gage_store::NoteValue;

        let backend = test_backend().await;
        let mut doc = doc_with_alternating_entries(10);
        // A note on entry index 0, anchored at its line
        doc.add_note(Note {
            id: "n0".into(),
            name: "comment".into(),
            value: NoteValue::Text("on entry 0".into()),
            author: "user:someone".into(),
            line: Some(doc.entries[0].line),
            line_end: None,
            metadata: None,
            created_ms: 0,
            modified_ms: 0,
        });
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, false);
        // Hide the assistant entries, the odd indices, so entry index 1
        // is filtered out while entry 0 and its note stay
        state.toggle_assistant();
        // Expand entry 0 so its note row sits between it and entry 1's slot
        let entry0_row = state
            .outline
            .rows()
            .iter()
            .position(|r| matches!(r.kind, RowKind::Entry { index: 0 }))
            .unwrap();
        state.outline.expand(entry0_row);
        // Entry number 2 is entry index 1, now hidden; the prior visible
        // entry is index 0, and its note row is not a valid landing
        type_text(&mut state, &backend, "2");
        press(&mut state, &backend, KeyCode::Char('g'));
        assert!(matches!(selected_kind(&state), RowKind::Entry { index: 0 }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_entry_with_no_earlier_visible_entry_is_a_no_op() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_alternating_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        // Hide the user entries, the even indices, including index 0
        state.toggle_user();
        let before = state.selected_entry_index();
        // Entry number 1 is entry index 0, now hidden, with nothing before
        type_text(&mut state, &backend, "1");
        press(&mut state, &backend, KeyCode::Char('g'));
        assert!(matches!(state.dialog, Dialog::None));
        assert_eq!(state.selected_entry_index(), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_entry_on_an_empty_outline_is_a_no_op() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_entries(0),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        type_text(&mut state, &backend, "5");
        press(&mut state, &backend, KeyCode::Char('g'));
        assert!(matches!(state.dialog, Dialog::None));
        assert_eq!(state.selected_entry_index(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_entry_cancels_without_moving_the_selection() {
        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        let before = state.selected_entry_index();
        type_text(&mut state, &backend, "4");
        press(&mut state, &backend, KeyCode::Esc);
        assert!(matches!(state.dialog, Dialog::None));
        assert_eq!(state.selected_entry_index(), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn goto_entry_hint_colors_the_keys_apart_from_the_labels() {
        use ratatui::style::Color;

        let backend = test_backend().await;
        let mut state = AppState::new(
            doc_with_entries(10),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        press(&mut state, &backend, KeyCode::Char('2'));
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();

        // The key color is cyan on the reversed dialog surface; the keys
        // "g" and "Esc" carry it and the labels "go" and "cancel" do not.
        assert_eq!(cell_bg(&terminal, "g go", 'g'), Color::Cyan);
        assert_ne!(cell_bg(&terminal, "g go", 'o'), Color::Cyan);
        assert_eq!(cell_bg(&terminal, "g go", 'E'), Color::Cyan);
        assert_ne!(cell_bg(&terminal, "g go", 'n'), Color::Cyan);
    }

    #[test]
    fn editor_hint_names_the_newline_shortcut_the_terminal_reports() {
        let doc = doc_with_alternating_entries(4);
        let mut state = AppState::new(doc, &ViewOptions::default(), DocSource::Backend, true);
        state.open_add_editor(None, "comment", "Add comment");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let legacy = screen(&terminal);
        assert!(legacy.contains("Alt+Enter or Ctrl+J newline"), "{legacy}");
        assert!(!legacy.contains("Shift+Enter"), "{legacy}");

        state.enhanced_keys = true;
        terminal.draw(|frame| draw(frame, &mut state)).unwrap();
        let enhanced = screen(&terminal);
        assert!(enhanced.contains("Shift+Enter newline"), "{enhanced}");
        assert!(!enhanced.contains("Alt+Enter"), "{enhanced}");
    }

    /// The loader sends the fetched page and the loading-dialog wake
    /// to the loop that drives the view; applying the page appends it
    /// to the open picker.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fetched_page_arrives_as_an_event_and_fills_the_picker() {
        // The fixture stays in scope: its temp dirs hold the store
        // the pages are read from.
        let fx = fixture();
        let backend = Backend::stored(fx.store).await;
        let (events, mut inbox) = unbounded_channel();
        let (picker, loader) = open_session_picker(&backend, None, events).unwrap();
        let loaded = picker.loaded();
        loader.start(0);
        let mut state = AppState::new(
            doc_with_entries(1),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        state.dialog = Dialog::OpenSession { picker, loader };

        let mut page = None;
        let mut loading_due = false;
        for _ in 0..2 {
            match inbox.recv().await.unwrap() {
                ViewEvent::Page(p) => page = Some(p),
                ViewEvent::LoadingDue => loading_due = true,
            }
        }
        assert!(loading_due);
        let page = page.unwrap().unwrap();
        assert_eq!(page.items.len(), 1);

        apply_event(&mut state, ViewEvent::Page(Ok(page))).unwrap();
        let Dialog::OpenSession { picker, .. } = &state.dialog else {
            panic!("picker should stay open");
        };
        assert_eq!(picker.loaded(), loaded + 1);
    }

    /// A page that lands after the picker closed has nothing to
    /// update; a page that failed is the loop's error either way.
    #[test]
    fn a_page_after_the_picker_closed_is_dropped_and_a_failed_page_is_an_error() {
        let mut state = AppState::new(
            doc_with_entries(1),
            &ViewOptions::default(),
            DocSource::Backend,
            false,
        );
        let page = Page {
            items: Vec::new(),
            has_more: false,
        };
        apply_event(&mut state, ViewEvent::Page(Ok(page))).unwrap();
        assert!(matches!(state.dialog, Dialog::None));

        let err =
            apply_event(&mut state, ViewEvent::Page(Err(io::Error::other("boom")))).unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }
}
