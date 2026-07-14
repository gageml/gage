//! Scan view — renders a [`ScanModel`]: overall progress, a tasks
//! table, a sessions table, and a results table.
//!
//! Two entry points share the rendering and key handling. [`run`]
//! drives a live scan: the caller runs the scan, adapts runner events
//! into [`Event`]s, and periodically reconciles notes/issues from the
//! db into [`Event::Results`]; the view applies them to the model and
//! lingers for inspection after the scan finishes. [`view`] renders an
//! already-complete model (a historical scan loaded from the db) with
//! no event source.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use unicode_width::UnicodeWidthStr;

use crate::dialog;
use crate::item_table::ItemTable;
use crate::scroll::ScrollView;
use crate::{markdown, message, styles};

/// A scan task identity, `{scanner}::{task}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskId {
    pub scanner: String,
    pub task: String,
}

/// One session selected for the scan.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    /// The session's JSONL source, read by the session dialog
    pub path: PathBuf,
}

/// Everything known before a live scan starts.
#[derive(Debug, Clone)]
pub struct ScanSetup {
    pub tasks: Vec<TaskId>,
    pub sessions: Vec<SessionEntry>,
}

/// The scan state the view renders. Built by [`ScanModel::new`] for a
/// live scan (events fill it in) or assembled from the db for a
/// historical one.
#[derive(Debug, Clone, Default)]
pub struct ScanModel {
    pub tasks: Vec<TaskItem>,
    pub sessions: Vec<SessionItem>,
    pub notes: Vec<NoteItem>,
    pub issues: Vec<IssueItem>,
    pub total: usize,
    pub progress: usize,
    pub errors: usize,
    pub finished: bool,
    /// Scan duration; None while a live scan is running.
    pub elapsed: Option<Duration>,
    /// The scan's captured stdout stream (`{scan_id}.out`), shown by
    /// the log dialog. None disables the dialog.
    pub out_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct TaskItem {
    pub id: TaskId,
    pub state: TaskState,
    pub elapsed: Option<Duration>,
    /// Live-scan dispatch time; drives the ticking elapsed display.
    pub started: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Completed,
    Error,
    Skipped,
}

impl TaskState {
    /// Sort rank: running, then pending, then finished
    fn rank(self) -> u8 {
        match self {
            TaskState::Running => 0,
            TaskState::Pending => 1,
            TaskState::Completed | TaskState::Error | TaskState::Skipped => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    /// JSONL source; None when the session is no longer on disk
    pub path: Option<PathBuf>,
    pub notes: usize,
    pub issues: usize,
}

/// A note written during the scan.
#[derive(Debug, Clone)]
pub struct NoteItem {
    pub id: String,
    pub name: String,
    /// One-line display form for the notes table
    pub value: String,
    /// Full value text for the detail view
    pub value_full: String,
    /// Target URI, e.g. `session:{id}`
    pub target: String,
    /// Short display form of the target for the notes table
    pub target_cell: String,
    pub author: String,
    /// Creation time display string
    pub created: String,
    pub explanation: Option<String>,
}

/// An issue opened during the scan.
#[derive(Debug, Clone)]
pub struct IssueItem {
    pub id: String,
    pub name: String,
    pub title: String,
    /// Status display string, e.g. `open` or `closed (resolved)`
    pub status: String,
    pub author: String,
    /// Creation time display string
    pub created: String,
    pub description: Option<String>,
    pub evidence: Vec<EvidenceItem>,
    pub events: Vec<EventItem>,
}

/// A note recorded as evidence for an issue.
#[derive(Debug, Clone)]
pub struct EvidenceItem {
    pub id: String,
    pub name: String,
    pub target: String,
    pub value: String,
}

/// One entry from an issue's event log.
#[derive(Debug, Clone)]
pub struct EventItem {
    /// Event type, e.g. `open`, `close`, `comment`
    pub kind: String,
    pub author: String,
    /// Timestamp display string
    pub timestamp: String,
    pub message: Option<String>,
}

impl ScanModel {
    /// Starting state for a live scan: every task pending, counts zero.
    pub fn new(setup: ScanSetup) -> Self {
        let tasks: Vec<TaskItem> = setup
            .tasks
            .into_iter()
            .map(|id| TaskItem {
                id,
                state: TaskState::Pending,
                started: None,
                elapsed: None,
            })
            .collect();
        let sessions = setup
            .sessions
            .into_iter()
            .map(|s| SessionItem {
                id: s.id,
                title: s.title,
                path: Some(s.path),
                notes: 0,
                issues: 0,
            })
            .collect();
        Self {
            total: tasks.len(),
            tasks,
            sessions,
            ..Self::default()
        }
    }

    fn apply(&mut self, event: &Event) {
        match event {
            Event::Status {
                total,
                progress,
                running,
            } => {
                self.total = *total;
                self.progress = *progress;
                for item in &mut self.tasks {
                    let is_running = running.contains(&item.id);
                    match item.state {
                        TaskState::Pending if is_running => {
                            item.state = TaskState::Running;
                            item.started = Some(Instant::now());
                        }
                        // No explicit completion event yet: a task that
                        // leaves the worker set without a Failed event
                        // is inferred completed.
                        TaskState::Running if !is_running => {
                            item.state = TaskState::Completed;
                            item.elapsed = item.started.map(|t| t.elapsed());
                        }
                        _ => {}
                    }
                }
            }
            Event::Failed { scanner, task, .. } => {
                self.errors += 1;
                if let Some(item) = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.id.scanner == *scanner && t.id.task == *task)
                {
                    item.state = TaskState::Error;
                    item.elapsed = item.started.map(|t| t.elapsed());
                }
            }
            Event::Results {
                notes,
                issues,
                sessions,
            } => {
                self.notes = notes.clone();
                self.issues = issues.clone();
                for counts in sessions {
                    if let Some(row) = self.sessions.iter_mut().find(|s| s.id == counts.id) {
                        row.notes = counts.notes;
                        row.issues = counts.issues;
                    }
                }
                self.sort_sessions();
            }
            Event::Log(_) | Event::Warning { .. } | Event::Finished => {}
        }
    }

    /// Stats-rank order: most issues, then most notes, then id.
    fn sort_sessions(&mut self) {
        self.sessions.sort_by(|a, b| {
            b.issues
                .cmp(&a.issues)
                .then_with(|| b.notes.cmp(&a.notes))
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    /// Task items in display order: running, pending, finished, each
    /// group sorted by name.
    fn sorted_tasks(&self) -> Vec<&TaskItem> {
        let mut items: Vec<&TaskItem> = self.tasks.iter().collect();
        items.sort_by(|a, b| {
            a.state
                .rank()
                .cmp(&b.state.rank())
                .then_with(|| a.id.scanner.cmp(&b.id.scanner))
                .then_with(|| a.id.task.cmp(&b.id.task))
        });
        items
    }
}

/// Per-session note/issue counts delivered with a [`Event::Results`]
/// refresh.
#[derive(Debug, Clone)]
pub struct SessionCounts {
    pub id: String,
    pub notes: usize,
    pub issues: usize,
}

/// Events the view consumes while a live scan runs.
#[derive(Debug, Clone)]
pub enum Event {
    /// Self-contained progress snapshot: task totals plus the set of
    /// tasks currently assigned to workers.
    Status {
        total: usize,
        progress: usize,
        running: Vec<TaskId>,
    },
    /// One scanner output line.
    Log(String),
    /// Non-fatal per-task warning.
    Warning {
        scanner: String,
        task: String,
        message: String,
    },
    /// Task failure with multi-line detail.
    Failed {
        scanner: String,
        task: String,
        message: String,
    },
    /// Refreshed scan results, reconciled from the db by the caller.
    /// Replaces the model's notes and issues wholesale and updates
    /// per-session counts.
    Results {
        notes: Vec<NoteItem>,
        issues: Vec<IssueItem>,
        sessions: Vec<SessionCounts>,
    },
    /// The scan is over; the view stays up until the user quits.
    Finished,
}

/// Show a live scan, applying events to the model as they arrive.
pub async fn run(model: ScanModel, mut events: UnboundedReceiver<Event>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, model, &mut events).await;
    ratatui::restore();
    result
}

/// Show an already-complete model (a historical scan).
pub async fn view(mut model: ScanModel) -> io::Result<()> {
    model.finished = true;
    let (tx, mut events) = unbounded_channel();
    drop(tx);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, model, &mut events).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    model: ScanModel,
    events: &mut UnboundedReceiver<Event>,
) -> io::Result<()> {
    let mut state = ViewState::new(model);
    let stop_input = Arc::new(AtomicBool::new(false));
    let mut input = spawn_input_thread(Arc::clone(&stop_input));
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut events_closed = false;
    loop {
        state.promote_pending_dialog();
        terminal.draw(|frame| draw(frame, &mut state))?;
        tokio::select! {
            ev = events.recv(), if !events_closed => {
                match ev {
                    Some(ev) => {
                        state.apply(ev);
                        while let Ok(ev) = events.try_recv() {
                            state.apply(ev);
                        }
                    }
                    None => {
                        state.mark_finished();
                        events_closed = true;
                    }
                }
            }
            ev = input.recv() => {
                match ev {
                    Some(TermEvent::Key(key))
                        if key.kind == KeyEventKind::Press
                            && handle_key(&mut state, key) => break,
                    // Input thread died (terminal input unavailable);
                    // without keys the view can never be dismissed, so
                    // exit rather than trap the user.
                    None => break,
                    _ => {}
                }
            }
            _ = tick.tick() => state.refresh_log(),
        }
    }
    stop_input.store(true, Ordering::Relaxed);
    Ok(())
}

/// Reads terminal input on a dedicated thread. `event::read` blocks,
/// so the thread polls with a timeout and checks `stop` between polls
/// to exit promptly once the view closes.
fn spawn_input_thread(stop: Arc<AtomicBool>) -> UnboundedReceiver<TermEvent> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            // A poll/read error means terminal input is gone (closed
            // tty); the view exits on the dropped channel.
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    rx
}

/// Returns true when the view should close.
fn handle_key(state: &mut ViewState, key: KeyEvent) -> bool {
    match &state.dialog {
        Dialog::ConfirmQuit => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return true,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    state.dialog = Dialog::None;
                }
                _ => {}
            }
            return false;
        }
        Dialog::ScanDone => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                state.dialog = Dialog::None;
            }
            return false;
        }
        Dialog::Note { .. }
        | Dialog::Issue { .. }
        | Dialog::Session { .. }
        | Dialog::Log { .. } => {
            let page = state.scroll_view.page() as isize;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => state.dialog = Dialog::None,
                KeyCode::Down | KeyCode::Char('j') => state.scroll_view.scroll_by(1),
                KeyCode::Up | KeyCode::Char('k') => state.scroll_view.scroll_by(-1),
                KeyCode::PageDown => state.scroll_view.scroll_by(page),
                KeyCode::PageUp => state.scroll_view.scroll_by(-page),
                KeyCode::Char('g') => state.scroll_view.scroll_to_top(),
                KeyCode::Char('G') => state.scroll_view.scroll_to_bottom(),
                KeyCode::Right => state.step_dialog_item(1),
                KeyCode::Left => state.step_dialog_item(-1),
                _ => {}
            }
            return false;
        }
        Dialog::None => {}
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return state.request_quit();
        }
        KeyCode::Char('q') | KeyCode::Esc => return state.request_quit(),
        KeyCode::Tab => state.cycle_focus(1),
        KeyCode::BackTab => state.cycle_focus(-1),
        KeyCode::Down | KeyCode::Char('j') => state.select_by(1),
        KeyCode::Up | KeyCode::Char('k') => state.select_by(-1),
        KeyCode::PageDown => state.select_by(state.page() as isize),
        KeyCode::PageUp => state.select_by(-(state.page() as isize)),
        KeyCode::Char('g') => state.select_first(),
        KeyCode::Char('G') => state.select_last(),
        KeyCode::Enter if state.focus == Focus::Notes => state.open_selected_note(),
        KeyCode::Enter if state.focus == Focus::Issues => state.open_selected_issue(),
        KeyCode::Enter if state.focus == Focus::Sessions => state.open_selected_session(),
        KeyCode::Char('l') => state.open_log(),
        _ => {}
    }
    false
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Focus {
    Tasks,
    Sessions,
    Notes,
    Issues,
}

/// Content-fit column width: widest value (or the header), capped at a
/// third of the panel so one long value can't starve the fill columns.
fn fit_col<'a>(header: &str, values: impl Iterator<Item = &'a str>, area: Rect) -> Constraint {
    let widest = values
        .map(|v| v.width())
        .max()
        .unwrap_or(0)
        .max(header.width());
    let cap = (area.width / 3) as usize;
    Constraint::Length(widest.min(cap) as u16)
}

const LOG_CAP: usize = 500;

struct ViewState {
    model: ScanModel,
    focus: Focus,
    tasks: ItemTable,
    sessions: ItemTable,
    notes: ItemTable,
    issues: ItemTable,
    log: VecDeque<String>,
    started: Instant,
    dialog: Dialog,
    /// A live scan finished while another dialog was open; ScanDone
    /// shows once that dialog closes
    scan_done_pending: bool,
    /// Scroll state and layout cache for the open content dialog
    scroll_view: ScrollView,
}

enum Dialog {
    None,
    /// Quit requested mid-scan; y stops the scan
    ConfirmQuit,
    /// Zoomed note detail. Holds a snapshot of the item so a results
    /// refresh can't shift what's being read.
    Note {
        note: NoteItem,
    },
    /// Zoomed issue detail; a snapshot, like Note
    Issue {
        issue: Box<IssueItem>,
    },
    /// Zoomed session contents, read from the JSONL at open. Section
    /// headers are padded to the dialog width at draw time, so the
    /// content is kept structured rather than pre-flattened.
    Session {
        id: String,
        content: SessionContent,
    },
    /// Captured scan streams (`{scan_id}.{err,out,log}`), reloaded
    /// from the files while a live scan runs.
    Log {
        content: Vec<Line<'static>>,
        loaded: Instant,
    },
    /// A live scan just finished
    ScanDone,
}

impl ViewState {
    fn new(model: ScanModel) -> Self {
        let mut state = Self {
            tasks: ItemTable::new(),
            sessions: ItemTable::new(),
            notes: ItemTable::new(),
            issues: ItemTable::new(),
            focus: Focus::Tasks,
            log: VecDeque::new(),
            started: Instant::now(),
            dialog: Dialog::None,
            scan_done_pending: false,
            scroll_view: ScrollView::new(),
            model,
        };
        state.sync_tables();
        state
    }

    /// Reconcile every table's selection with the model after a
    /// change — data replacement or re-sort moves rows under the
    /// positional table state; the tables re-anchor by item id.
    fn sync_tables(&mut self) {
        let task_ids = sorted_task_ids(&self.model);
        let refs: Vec<&str> = task_ids.iter().map(String::as_str).collect();
        self.tasks.update(&refs);
        let ids: Vec<&str> = self.model.sessions.iter().map(|s| s.id.as_str()).collect();
        self.sessions.update(&ids);
        let ids: Vec<&str> = self.model.notes.iter().map(|n| n.id.as_str()).collect();
        self.notes.update(&ids);
        let ids: Vec<&str> = self.model.issues.iter().map(|i| i.id.as_str()).collect();
        self.issues.update(&ids);
    }

    fn open_selected_note(&mut self) {
        let Some(i) = self.notes.selected_index() else {
            return;
        };
        if let Some(note) = self.model.notes.get(i) {
            self.scroll_view.reset();
            self.dialog = Dialog::Note { note: note.clone() };
        }
    }

    fn open_selected_issue(&mut self) {
        let Some(i) = self.issues.selected_index() else {
            return;
        };
        if let Some(issue) = self.model.issues.get(i) {
            self.scroll_view.reset();
            self.dialog = Dialog::Issue {
                issue: Box::new(issue.clone()),
            };
        }
    }

    /// Move the source table's selection while an item dialog is open
    /// and reload the dialog with the newly selected item. The log
    /// dialog has no backing table and is unaffected.
    fn step_dialog_item(&mut self, delta: isize) {
        match self.dialog {
            Dialog::Note { .. } => {
                let ids: Vec<&str> = self.model.notes.iter().map(|n| n.id.as_str()).collect();
                self.notes.select_by(delta, &ids);
                self.open_selected_note();
            }
            Dialog::Issue { .. } => {
                let ids: Vec<&str> = self.model.issues.iter().map(|i| i.id.as_str()).collect();
                self.issues.select_by(delta, &ids);
                self.open_selected_issue();
            }
            Dialog::Session { .. } => {
                let ids: Vec<&str> = self.model.sessions.iter().map(|s| s.id.as_str()).collect();
                self.sessions.select_by(delta, &ids);
                self.open_selected_session();
            }
            _ => {}
        }
    }

    fn open_selected_session(&mut self) {
        let Some(i) = self.sessions.selected_index() else {
            return;
        };
        let Some(session) = self.model.sessions.get(i) else {
            return;
        };
        self.scroll_view.reset();
        self.dialog = Dialog::Session {
            id: session.id.clone(),
            content: session_content(session),
        };
    }

    fn open_log(&mut self) {
        if self.model.out_path.is_none() {
            return;
        }
        self.scroll_view.reset();
        self.dialog = Dialog::Log {
            content: self.read_log(),
            loaded: Instant::now(),
        };
    }

    /// Reload the log dialog's content while the scan is live, at most
    /// once a second. The scroll position is preserved.
    fn refresh_log(&mut self) {
        if self.model.finished {
            return;
        }
        let due = match &self.dialog {
            Dialog::Log { loaded, .. } => loaded.elapsed() >= Duration::from_secs(1),
            _ => false,
        };
        if !due {
            return;
        }
        let content = self.read_log();
        if let Dialog::Log {
            content: current,
            loaded,
        } = &mut self.dialog
        {
            *current = content;
            *loaded = Instant::now();
            self.scroll_view.invalidate();
        }
    }

    /// Read the scan's captured streams — `.err` (in red), `.out`,
    /// then `.log`, separated by a blank line. Absent or empty files
    /// are skipped; the files are created lazily, so a scan may simply
    /// have produced nothing.
    fn read_log(&self) -> Vec<Line<'static>> {
        let Some(out_path) = &self.model.out_path else {
            return Vec::new();
        };
        let mut lines: Vec<Line<'static>> = Vec::new();
        for ext in ["err", "out", "log"] {
            let Ok(content) = std::fs::read_to_string(out_path.with_extension(ext)) else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.extend(content.lines().map(|l| match ext {
                "err" => Line::from(Span::styled(l.to_string(), styles::LogLevel::error())),
                "log" => log_line(l),
                _ => Line::raw(l.to_string()),
            }));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("(no output)", styles::Text::dim())));
        }
        lines
    }

    /// Quitting a finished view is immediate; quitting mid-scan stops
    /// the scan, so it requires confirmation first.
    fn request_quit(&mut self) -> bool {
        if self.model.finished {
            return true;
        }
        self.dialog = Dialog::ConfirmQuit;
        false
    }

    /// Record completion and announce it. The caller sends a final db
    /// reconcile ahead of the Finished event, so the results on screen
    /// are current when the dialog appears. Historical views start
    /// finished and never transition. When another dialog is up the
    /// announcement is deferred until it closes — completion still
    /// shows in the header and footer meanwhile.
    fn mark_finished(&mut self) {
        if !self.model.finished {
            self.model.finished = true;
            self.model.elapsed = Some(self.started.elapsed());
            if matches!(self.dialog, Dialog::None) {
                self.dialog = Dialog::ScanDone;
            } else {
                self.scan_done_pending = true;
            }
        }
    }

    /// Show a deferred ScanDone once no other dialog is open. Run each
    /// loop iteration before drawing.
    fn promote_pending_dialog(&mut self) {
        if self.scan_done_pending && matches!(self.dialog, Dialog::None) {
            self.scan_done_pending = false;
            self.dialog = Dialog::ScanDone;
        }
    }

    fn apply(&mut self, event: Event) {
        self.model.apply(&event);
        self.sync_tables();
        match event {
            Event::Log(line) => self.push_log(line),
            Event::Warning {
                scanner,
                task,
                message,
            } => self.push_log(format!("warning: {scanner}::{task}: {message}")),
            Event::Failed {
                scanner,
                task,
                message,
            } => {
                self.push_log(format!("error: {scanner}::{task}"));
                for line in message.lines() {
                    self.push_log(format!("  {line}"));
                }
            }
            Event::Finished => self.mark_finished(),
            Event::Status { .. } | Event::Results { .. } => {}
        }
    }

    fn push_log(&mut self, line: String) {
        while self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn cycle_focus(&mut self, dir: isize) {
        self.focus = match (self.focus, dir >= 0) {
            (Focus::Tasks, true) | (Focus::Notes, false) => Focus::Sessions,
            (Focus::Sessions, true) | (Focus::Issues, false) => Focus::Notes,
            (Focus::Notes, true) | (Focus::Tasks, false) => Focus::Issues,
            (Focus::Issues, true) | (Focus::Sessions, false) => Focus::Tasks,
        };
    }

    /// Page size for the focused table — its last drawn viewport
    fn page(&self) -> usize {
        match self.focus {
            Focus::Tasks => self.tasks.page(),
            Focus::Sessions => self.sessions.page(),
            Focus::Notes => self.notes.page(),
            Focus::Issues => self.issues.page(),
        }
    }

    fn select_by(&mut self, delta: isize) {
        self.focused_apply(|table, ids| table.select_by(delta, ids));
    }

    fn select_first(&mut self) {
        self.focused_apply(ItemTable::select_first);
    }

    fn select_last(&mut self) {
        self.focused_apply(ItemTable::select_last);
    }

    /// Run a selection operation on the focused table with its
    /// display-order id list.
    fn focused_apply(&mut self, op: impl Fn(&mut ItemTable, &[&str])) {
        match self.focus {
            Focus::Tasks => {
                let ids = sorted_task_ids(&self.model);
                let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                op(&mut self.tasks, &refs);
            }
            Focus::Sessions => {
                let ids: Vec<&str> = self.model.sessions.iter().map(|s| s.id.as_str()).collect();
                op(&mut self.sessions, &ids);
            }
            Focus::Notes => {
                let ids: Vec<&str> = self.model.notes.iter().map(|n| n.id.as_str()).collect();
                op(&mut self.notes, &ids);
            }
            Focus::Issues => {
                let ids: Vec<&str> = self.model.issues.iter().map(|i| i.id.as_str()).collect();
                op(&mut self.issues, &ids);
            }
        }
    }
}

/// Task ids in display (sorted) order — tasks have no single id
/// column, so identity is the `{scanner}::{task}` pair.
fn sorted_task_ids(model: &ScanModel) -> Vec<String> {
    model
        .sorted_tasks()
        .iter()
        .map(|t| format!("{}::{}", t.id.scanner, t.id.task))
        .collect()
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [progress, tasks, sessions, notes, issues, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Fill(2),
        Constraint::Fill(2),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_progress(frame, progress, state);
    draw_tasks(frame, tasks, state);
    draw_sessions(frame, sessions, state);
    draw_notes(frame, notes, state);
    draw_issues(frame, issues, state);
    draw_footer(frame, footer, state);
    // Split borrows: the dialog holds the content, the scroll view
    // holds position and layout cache
    let ViewState {
        dialog,
        scroll_view,
        model,
        ..
    } = state;
    match dialog {
        Dialog::ConfirmQuit => draw_confirm_quit(frame),
        Dialog::Note { note } => {
            scroll_view.render_modal(frame, format!(" Note {} ", note.id), |_| {
                vec![note_lines(note)]
            })
        }
        Dialog::Issue { issue } => {
            scroll_view.render_modal(frame, format!(" Issue {} ", issue.id), |_| {
                vec![issue_lines(issue)]
            })
        }
        Dialog::Session { id, content } => {
            scroll_view.render_modal(frame, format!(" Session {id} "), |width| {
                session_sections(content, width as usize)
            })
        }
        Dialog::Log { content, .. } => {
            scroll_view.render_modal(frame, " Log ".to_string(), |_| vec![content.clone()]);
        }
        Dialog::ScanDone => draw_scan_done(frame, model.elapsed),
        Dialog::None => {}
    }
}

/// Note detail content: caption/value header columns, then the value
/// and explanation rendered as markdown.
fn note_lines(note: &NoteItem) -> Vec<Line<'static>> {
    let mut lines = header_lines(&[
        ("Name", &note.name),
        ("Target", &note.target),
        ("Author", &note.author),
        ("Created", &note.created),
    ]);
    lines.push(Line::raw(""));
    lines.extend(markdown::render(&note.value_full));
    if let Some(explanation) = &note.explanation {
        section(&mut lines, "Explanation", markdown::render(explanation));
    }
    lines
}

/// Issue detail content, mirroring `gage issue show`.
fn issue_lines(issue: &IssueItem) -> Vec<Line<'static>> {
    let mut lines = header_lines(&[
        ("Name", &issue.name),
        ("Status", &issue.status),
        ("Author", &issue.author),
        ("Created", &issue.created),
    ]);
    lines.push(Line::raw(""));
    lines.push(Line::raw(issue.title.clone()));
    if let Some(description) = &issue.description {
        lines.push(Line::raw(""));
        lines.extend(markdown::render(description));
    }

    if !issue.evidence.is_empty() {
        let mut content: Vec<Line<'static>> = Vec::new();
        for (i, ev) in issue.evidence.iter().enumerate() {
            if i > 0 {
                content.push(Line::raw(""));
            }
            content.push(Line::from(Span::styled(
                format!("{} · {} · {}", ev.id, ev.name, ev.target),
                styles::Text::dim(),
            )));
            content.extend(
                ev.value
                    .lines()
                    .map(|l| Line::from(Span::styled(l.to_string(), styles::Text::accent()))),
            );
        }
        section(&mut lines, "Evidence", content);
    }

    if !issue.events.is_empty() {
        let mut content: Vec<Line<'static>> = Vec::new();
        for (i, ev) in issue.events.iter().enumerate() {
            if i > 0 {
                content.push(Line::raw(""));
            }
            content.push(Line::from(Span::styled(
                format!("{} · {} · {}", ev.kind, ev.author, ev.timestamp),
                styles::Text::dim(),
            )));
            if let Some(message) = &ev.message {
                content.extend(message.lines().map(|l| Line::raw(l.to_string())));
            }
        }
        section(&mut lines, "Events", content);
    }

    lines
}

/// Session dialog content: intro lines (header attributes and any
/// availability notice), message sections, and an optional trailing
/// notice (truncation or read error).
struct SessionContent {
    intro: Vec<Line<'static>>,
    sections: Vec<SessionSection>,
    notice: Option<String>,
}

/// One message section: `{line} {label}` header over the rendered body.
struct SessionSection {
    line_num: u32,
    label: String,
    body: Vec<Line<'static>>,
}

/// Read a session's contents for the dialog: header attributes, then
/// one section per message — the data layer's message predicate
/// (`is_message_row`). Section labels use the same function as the
/// session viewer's outline ([`crate::doc::Entry::label`]).
fn session_content(session: &SessionItem) -> SessionContent {
    let notes = session.notes.to_string();
    let issues = session.issues.to_string();
    let mut content = SessionContent {
        intro: header_lines(&[
            ("Title", &session.title),
            ("Notes", &notes),
            ("Issues", &issues),
        ]),
        sections: Vec::new(),
        notice: None,
    };

    let Some(path) = &session.path else {
        content.notice = Some("(session file unavailable)".to_string());
        return content;
    };
    let reader = match gage_claude::session_reader::SessionReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            content.notice = Some(format!("(cannot read session: {e})"));
            return content;
        }
    };

    for result in reader {
        let (line_num, value) = match result {
            Ok(pair) => pair,
            Err(e) => {
                content.notice = Some(format!("(read error: {e})"));
                break;
            }
        };
        if !gage_index::is_message_row(&value) {
            continue;
        }
        let entry = crate::doc::Entry {
            line: line_num,
            value,
        };
        let body = entry.message().map(message::render).unwrap_or_default();
        content.sections.push(SessionSection {
            line_num,
            label: entry.label().to_string(),
            body,
        });
    }
    content
}

/// Session dialog sections: intro (header attributes), one section
/// per message — a full-width header bar (dim line number, label, one
/// column of inner padding each side) over a blank line and the
/// rendered body — and any trailing notice.
fn session_sections(content: &SessionContent, width: usize) -> Vec<Vec<Line<'static>>> {
    let mut out = Vec::with_capacity(content.sections.len() + 2);
    out.push(content.intro.clone());
    for section in &content.sections {
        let number = format!(" {} ", section.line_num);
        let label_width = width.saturating_sub(number.width());
        let mut lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(number, styles::Text::header_dim()),
                Span::styled(
                    format!("{:<label_width$}", section.label),
                    styles::Text::header(),
                ),
            ]),
            Line::raw(""),
        ];
        lines.extend(section.body.iter().cloned());
        out.push(lines);
    }
    if let Some(notice) = &content.notice {
        out.push(vec![
            Line::raw(""),
            Line::from(Span::styled(notice.clone(), styles::Text::dim())),
        ]);
    }
    out
}

/// Header attributes as caption/value columns; the caption column pads
/// to the widest caption.
fn header_lines(headers: &[(&'static str, &str)]) -> Vec<Line<'static>> {
    let caption_width = headers.iter().map(|(c, _)| c.width()).max().unwrap_or(0);
    headers
        .iter()
        .map(|(caption, value)| {
            Line::from(vec![
                Span::styled(
                    format!("{caption:<width$}  ", width = caption_width),
                    styles::Text::dim(),
                ),
                Span::raw((*value).to_string()),
            ])
        })
        .collect()
}

/// A captioned page section separated from what precedes it by a
/// blank line.
fn section(lines: &mut Vec<Line<'static>>, caption: &'static str, content: Vec<Line<'static>>) {
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(caption, styles::Text::dim())));
    lines.extend(content);
}

/// Style a tracing log line per field. The files are written without
/// ANSI codes; color is applied here against the default fmt layout
/// `{timestamp} {LEVEL} {target}: {body}`: dim timestamp, the level in
/// its status color, dim target, plain body. Panics from the gage-log
/// hook fit the same shape without a target. Lines that don't open
/// with a level in the second slot (e.g. backtrace continuations)
/// render plain.
fn log_line(line: &str) -> Line<'static> {
    let tokens = token_ranges(line, 3);
    let Some(&(level_start, level_end)) = tokens.get(1) else {
        return Line::raw(line.to_string());
    };
    let level = &line[level_start..level_end];
    let level_style = match level {
        "ERROR" | "PANIC" => Some(styles::LogLevel::error()),
        "WARN" => Some(styles::LogLevel::warn()),
        "INFO" => Some(styles::LogLevel::info()),
        "DEBUG" | "TRACE" => Some(styles::LogLevel::debug()),
        _ => return Line::raw(line.to_string()),
    };

    let mut spans = vec![
        Span::styled(line[..level_start].to_string(), styles::Text::dim()),
        match level_style {
            Some(st) => Span::styled(level.to_string(), st),
            None => Span::raw(level.to_string()),
        },
    ];
    // The target field is `:`-terminated; without one (e.g. a PANIC
    // line) everything after the level is body
    match tokens.get(2) {
        Some(&(_, target_end)) if line[..target_end].ends_with(':') => {
            spans.push(Span::styled(
                line[level_end..target_end].to_string(),
                styles::Text::dim(),
            ));
            spans.push(Span::raw(line[target_end..].to_string()));
        }
        _ => spans.push(Span::raw(line[level_end..].to_string())),
    }
    Line::from(spans)
}

/// Byte ranges of the first `n` whitespace-delimited tokens.
fn token_ranges(line: &str, n: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push((s, i));
                if out.len() == n {
                    return out;
                }
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, line.len()));
    }
    out
}

fn draw_confirm_quit(frame: &mut Frame) {
    dialog::draw_message(frame, "Stop the current scan?", "y / n");
}

fn draw_scan_done(frame: &mut Frame, elapsed: Option<Duration>) {
    let message = match elapsed {
        Some(e) => format!("Scan completed in {}", fmt_duration(e)),
        None => "Scan completed".to_string(),
    };
    dialog::draw_message(frame, &message, "Close");
}

fn draw_progress(frame: &mut Frame, area: Rect, state: &ViewState) {
    let model = &state.model;
    let counts = Line::from(vec![
        Span::styled("  Notes ", styles::Text::dim()),
        Span::raw(model.notes.len().to_string()),
        Span::styled("  Issues ", styles::Text::dim()),
        Span::raw(model.issues.len().to_string()),
        Span::styled("  Errors ", styles::Text::dim()),
        if model.errors > 0 {
            Span::styled(model.errors.to_string(), styles::RunStatus::error())
        } else {
            Span::raw("0")
        },
    ]);
    let [tasks, badges] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(counts.width() as u16),
    ])
    .areas(area);

    let ratio = if model.total > 0 {
        (model.progress as f64 / model.total as f64).min(1.0)
    } else {
        0.0
    };
    // A finished model without a duration (a historical scan that never
    // completed) shows no time rather than a ticking one.
    let label = match (model.finished, model.elapsed) {
        (true, Some(e)) => format!(
            "{}/{} · done {}",
            model.progress,
            model.total,
            fmt_duration(e)
        ),
        (true, None) => format!("{}/{}", model.progress, model.total),
        (false, _) => format!(
            "{}/{} · {}",
            model.progress,
            model.total,
            fmt_duration(state.started.elapsed())
        ),
    };
    frame.render_widget(
        Gauge::default()
            .gauge_style(styles::Panel::gauge())
            .ratio(ratio)
            .label(label),
        tasks,
    );

    frame.render_widget(Paragraph::new(counts), badges);
}

/// Indicatif-style braille spinner; one frame per 100ms redraw tick
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn draw_tasks(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let frame_idx = (state.started.elapsed().as_millis() / 100) as usize % SPINNER.len();
    let spinner = *SPINNER.get(frame_idx).expect("mod len is in bounds");
    let selected = state.tasks.selected_index();
    let rows: Vec<Row> = state
        .model
        .sorted_tasks()
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let glyph = match t.state {
                TaskState::Pending => "□",
                TaskState::Running => spinner,
                TaskState::Completed => "✓",
                TaskState::Error => "✗",
                TaskState::Skipped => "⊘",
            };
            let (label, label_style) = match t.state {
                TaskState::Pending => ("pending", styles::RunStatus::pending()),
                TaskState::Running => ("running", styles::RunStatus::running()),
                TaskState::Completed => ("done", styles::RunStatus::completed()),
                TaskState::Error => ("error", styles::RunStatus::error()),
                TaskState::Skipped => ("skipped", styles::RunStatus::skipped()),
            };
            // Colored/dim cells on the selected row would invert into
            // per-cell backgrounds under the REVERSED highlight
            let status = if selected == Some(i) {
                Span::raw(label)
            } else {
                Span::styled(label, label_style)
            };
            let time = match t.state {
                TaskState::Running => t
                    .started
                    .map(|s| fmt_duration(s.elapsed()))
                    .unwrap_or_default(),
                _ => t.elapsed.map(fmt_duration).unwrap_or_default(),
            };
            Row::new(vec![
                Cell::from(format!("{glyph} {}", t.id.scanner)),
                Cell::from(t.id.task.clone()),
                Cell::from(status),
                Cell::from(time),
            ])
        })
        .collect();
    let count = rows.len();
    // Widen for the "<glyph> " prefix on each scanner cell
    let scanner_col = match fit_col(
        "Scanner",
        state.model.tasks.iter().map(|t| t.id.scanner.as_str()),
        area,
    ) {
        Constraint::Length(w) => Constraint::Length(w + 2),
        c => c,
    };
    let table = Table::new(
        rows,
        [
            scanner_col,
            Constraint::Fill(1),
            Constraint::Length(8),
            Constraint::Length(7),
        ],
    )
    .header(header_row(["Scanner", "Task", "Status", "Time"]))
    .row_highlight_style(styles::Panel::selection(state.focus == Focus::Tasks))
    .block(panel_block(
        format!(" Tasks ({count}) "),
        state.focus == Focus::Tasks,
    ));
    state
        .tasks
        .render(frame, area, table, count, state.focus == Focus::Tasks);
}

fn draw_sessions(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.sessions.selected_index();
    let rows: Vec<Row> = state
        .model
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            Row::new(vec![
                Cell::from(id_span(&s.id, selected == Some(i))),
                Cell::from(s.title.clone()),
                Cell::from(s.notes.to_string()),
                Cell::from(s.issues.to_string()),
            ])
        })
        .collect();
    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Length(6),
            Constraint::Length(7),
        ],
    )
    .header(header_row(["Id", "Title", "Notes", "Issues"]))
    .row_highlight_style(styles::Panel::selection(state.focus == Focus::Sessions))
    .block(panel_block(
        format!(" Sessions ({count}) "),
        state.focus == Focus::Sessions,
    ));
    state
        .sessions
        .render(frame, area, table, count, state.focus == Focus::Sessions);
}

fn draw_notes(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.notes.selected_index();
    let rows: Vec<Row> = state
        .model
        .notes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            Row::new(vec![
                Cell::from(id_span(&n.id, selected == Some(i))),
                Cell::from(n.name.clone()),
                Cell::from(n.value.clone()),
                Cell::from(n.target_cell.clone()),
            ])
        })
        .collect();
    let count = rows.len();
    let name_col = fit_col(
        "Name",
        state.model.notes.iter().map(|n| n.name.as_str()),
        area,
    );
    let target_col = fit_col(
        "Target",
        state.model.notes.iter().map(|n| n.target_cell.as_str()),
        area,
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            name_col,
            Constraint::Fill(1),
            target_col,
        ],
    )
    .header(header_row(["Id", "Name", "Value", "Target"]))
    .row_highlight_style(styles::Panel::selection(state.focus == Focus::Notes))
    .block(panel_block(
        format!(" Notes ({count}) "),
        state.focus == Focus::Notes,
    ));
    state
        .notes
        .render(frame, area, table, count, state.focus == Focus::Notes);
}

fn draw_issues(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.issues.selected_index();
    let rows: Vec<Row> = state
        .model
        .issues
        .iter()
        .enumerate()
        .map(|(idx, i)| {
            Row::new(vec![
                Cell::from(id_span(&i.id, selected == Some(idx))),
                Cell::from(i.name.clone()),
                Cell::from(i.title.clone()),
            ])
        })
        .collect();
    let count = rows.len();
    let name_col = fit_col(
        "Name",
        state.model.issues.iter().map(|i| i.name.as_str()),
        area,
    );
    let table = Table::new(rows, [Constraint::Length(8), name_col, Constraint::Fill(1)])
        .header(header_row(["Id", "Name", "Title"]))
        .row_highlight_style(styles::Panel::selection(state.focus == Focus::Issues))
        .block(panel_block(
            format!(" Issues ({count}) "),
            state.focus == Focus::Issues,
        ));
    state
        .issues
        .render(frame, area, table, count, state.focus == Focus::Issues);
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &ViewState) {
    let help = if state.model.finished {
        "scan complete · q quit · Tab cycle · ↑/↓ select · l log"
    } else {
        "q quit · Tab cycle · ↑/↓ select · l log"
    };
    let help_width = help.width() as u16;
    let [_, help_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(help_width),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Span::styled(help, styles::Panel::footer())),
        help_area,
    );
}

fn panel_block(title: String, active: bool) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(styles::Panel::border(active))
}

fn header_row<const N: usize>(names: [&'static str; N]) -> Row<'static> {
    Row::new(names.map(|n| Cell::from(Span::styled(n, styles::Text::dim()))))
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Id cell: dim, except on the selected row where dim text lacks
/// contrast against the highlight background.
fn id_span(id: &str, selected: bool) -> Span<'static> {
    if selected {
        Span::raw(short_id(id))
    } else {
        Span::styled(short_id(id), styles::Text::dim())
    }
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        format!("{}ms", d.as_millis())
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}
