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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, Gauge, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use unicode_width::UnicodeWidthStr;

use crate::{markdown, style};

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
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut events_closed = false;
    loop {
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
            _ = tick.tick() => {}
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
    let page = state.detail_page.max(1);
    let max_scroll = state.detail_max_scroll;
    match &mut state.dialog {
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
        Dialog::Note { scroll, .. } => {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => state.dialog = Dialog::None,
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = scroll.saturating_add(1).min(max_scroll);
                }
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(page).min(max_scroll),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
                KeyCode::Char('g') => *scroll = 0,
                KeyCode::Char('G') => *scroll = max_scroll,
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

/// Rows a table panel can display: panel height minus borders and the
/// header row.
fn table_viewport(area: Rect) -> usize {
    area.height.saturating_sub(3) as usize
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
    tasks_table: TableState,
    sessions_table: TableState,
    notes_table: TableState,
    issues_table: TableState,
    /// Visible row count per table, recorded at draw time so paging
    /// keys know the page size before the next frame
    tasks_viewport: usize,
    sessions_viewport: usize,
    notes_viewport: usize,
    issues_viewport: usize,
    log: VecDeque<String>,
    started: Instant,
    dialog: Dialog,
    /// Scroll geometry of the open detail dialog, recorded at draw
    /// time so scroll keys know the page size and limit
    detail_page: u16,
    detail_max_scroll: u16,
}

enum Dialog {
    None,
    /// Quit requested mid-scan; y stops the scan
    ConfirmQuit,
    /// Zoomed note detail. Holds a snapshot of the item so a results
    /// refresh can't shift what's being read.
    Note {
        note: NoteItem,
        scroll: u16,
    },
}

impl ViewState {
    fn new(model: ScanModel) -> Self {
        Self {
            tasks_table: initial_selection(model.tasks.len()),
            sessions_table: initial_selection(model.sessions.len()),
            notes_table: initial_selection(model.notes.len()),
            issues_table: initial_selection(model.issues.len()),
            tasks_viewport: 0,
            sessions_viewport: 0,
            notes_viewport: 0,
            issues_viewport: 0,
            focus: Focus::Tasks,
            log: VecDeque::new(),
            started: Instant::now(),
            dialog: Dialog::None,
            detail_page: 0,
            detail_max_scroll: 0,
            model,
        }
    }

    fn open_selected_note(&mut self) {
        let Some(i) = self.notes_table.selected() else {
            return;
        };
        if let Some(note) = self.model.notes.get(i) {
            self.dialog = Dialog::Note {
                note: note.clone(),
                scroll: 0,
            };
        }
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

    fn mark_finished(&mut self) {
        if !self.model.finished {
            self.model.finished = true;
            self.model.elapsed = Some(self.started.elapsed());
        }
    }

    fn apply(&mut self, event: Event) {
        self.model.apply(&event);
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
        let viewport = match self.focus {
            Focus::Tasks => self.tasks_viewport,
            Focus::Sessions => self.sessions_viewport,
            Focus::Notes => self.notes_viewport,
            Focus::Issues => self.issues_viewport,
        };
        viewport.max(1)
    }

    fn select_by(&mut self, delta: isize) {
        let (table, len) = self.focused_table();
        if len == 0 {
            table.select(None);
            return;
        }
        let next = match table.selected() {
            Some(current) => (current as isize + delta).clamp(0, len as isize - 1) as usize,
            None => 0,
        };
        table.select(Some(next));
    }

    fn select_first(&mut self) {
        let (table, len) = self.focused_table();
        table.select((len > 0).then_some(0));
    }

    fn select_last(&mut self) {
        let (table, len) = self.focused_table();
        table.select(len.checked_sub(1));
    }

    fn focused_table(&mut self) -> (&mut TableState, usize) {
        match self.focus {
            Focus::Tasks => (&mut self.tasks_table, self.model.tasks.len()),
            Focus::Sessions => (&mut self.sessions_table, self.model.sessions.len()),
            Focus::Notes => (&mut self.notes_table, self.model.notes.len()),
            Focus::Issues => (&mut self.issues_table, self.model.issues.len()),
        }
    }
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
    let detail_geometry = match &state.dialog {
        Dialog::ConfirmQuit => {
            draw_confirm_quit(frame);
            None
        }
        Dialog::Note { note, scroll } => Some(draw_note_detail(frame, note, *scroll)),
        Dialog::None => None,
    };
    if let Some((page, max_scroll)) = detail_geometry {
        state.detail_page = page;
        state.detail_max_scroll = max_scroll;
    }
}

/// Renders the note detail modal and returns `(page, max_scroll)` for
/// the scroll keys. Fields lay out as a page: caption above content,
/// value and explanation rendered as markdown.
fn draw_note_detail(frame: &mut Frame, note: &NoteItem, scroll: u16) -> (u16, u16) {
    let area = detail_rect(frame.area());
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(" Note {} ", note.id))
        .padding(Padding::horizontal(1));
    let body = block.inner(area);
    frame.render_widget(block, area);

    // Header attributes as caption/value columns; the caption column
    // pads to the widest caption
    let headers = [
        ("Name", note.name.as_str()),
        ("Target", note.target.as_str()),
        ("Author", note.author.as_str()),
        ("Created", note.created.as_str()),
    ];
    let caption_width = headers.iter().map(|(c, _)| c.width()).max().unwrap_or(0);
    let mut lines: Vec<Line> = headers
        .iter()
        .map(|(caption, value)| {
            Line::from(vec![
                Span::styled(
                    format!("{caption:<width$}  ", width = caption_width),
                    style::text_dim(),
                ),
                Span::raw((*value).to_string()),
            ])
        })
        .collect();
    let section =
        |lines: &mut Vec<Line<'static>>, caption: &'static str, content: Vec<Line<'static>>| {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(caption, style::text_dim())));
            lines.extend(content);
        };
    lines.push(Line::raw(""));
    lines.extend(markdown::render(&note.value_full));
    if let Some(explanation) = &note.explanation {
        section(&mut lines, "Explanation", markdown::render(explanation));
    }

    let scroll_width = body.width.saturating_sub(1);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(scroll_width)).unwrap_or(u16::MAX);
    let max_scroll = total.saturating_sub(body.height);
    let scroll = scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), body);

    let mut sb_state = ScrollbarState::new(max_scroll as usize).position(scroll as usize);
    frame.render_stateful_widget(
        scrollbar(true),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut sb_state,
    );
    (body.height, max_scroll)
}

/// Detail modals cover most of the frame, inset a few cells so the
/// main view remains visible behind them.
fn detail_rect(frame: Rect) -> Rect {
    let margin_x = (frame.width / 10).clamp(2, 8);
    let margin_y = (frame.height / 10).clamp(1, 3);
    frame.inner(Margin {
        horizontal: margin_x,
        vertical: margin_y,
    })
}

fn draw_confirm_quit(frame: &mut Frame) {
    let frame_area = frame.area();
    let width = 30u16.min(frame_area.width.saturating_sub(2));
    let height = 6u16.min(frame_area.height.saturating_sub(2));
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(Span::styled("Confirm", style::text_dim()))
        .border_style(style::text_dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [_, msg, _, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new("Stop the current scan?").centered(), msg);
    frame.render_widget(
        Paragraph::new(Span::styled("y / n", style::text_dim())).centered(),
        hint,
    );
}

fn draw_progress(frame: &mut Frame, area: Rect, state: &ViewState) {
    let model = &state.model;
    let counts = Line::from(vec![
        Span::styled("  Notes ", style::text_dim()),
        Span::raw(model.notes.len().to_string()),
        Span::styled("  Issues ", style::text_dim()),
        Span::raw(model.issues.len().to_string()),
        Span::styled("  Errors ", style::text_dim()),
        if model.errors > 0 {
            Span::styled(model.errors.to_string(), style::error())
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
            .gauge_style(style::gauge())
            .ratio(ratio)
            .label(label),
        tasks,
    );

    frame.render_widget(Paragraph::new(counts), badges);
}

fn draw_tasks(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.tasks_table.selected();
    let rows: Vec<Row> = state
        .model
        .sorted_tasks()
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (label, label_style) = match t.state {
                TaskState::Pending => ("pending", style::text_dim()),
                TaskState::Running => ("running", style::running()),
                TaskState::Completed => ("done", style::text_dim()),
                TaskState::Error => ("error", style::error()),
                TaskState::Skipped => ("skipped", style::text_dim()),
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
                Cell::from(t.id.scanner.clone()),
                Cell::from(t.id.task.clone()),
                Cell::from(status),
                Cell::from(time),
            ])
        })
        .collect();
    let count = rows.len();
    let scanner_col = fit_col(
        "Scanner",
        state.model.tasks.iter().map(|t| t.id.scanner.as_str()),
        area,
    );
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
    .row_highlight_style(highlight_style(state.focus == Focus::Tasks))
    .block(panel_block(
        format!(" Tasks ({count}) "),
        state.focus == Focus::Tasks,
    ));
    state.tasks_viewport = table_viewport(area);
    frame.render_stateful_widget(table, area, &mut state.tasks_table);
    draw_table_scrollbar(
        frame,
        area,
        count,
        &state.tasks_table,
        state.focus == Focus::Tasks,
    );
}

fn draw_sessions(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.sessions_table.selected();
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
    .row_highlight_style(highlight_style(state.focus == Focus::Sessions))
    .block(panel_block(
        format!(" Sessions ({count}) "),
        state.focus == Focus::Sessions,
    ));
    state.sessions_viewport = table_viewport(area);
    frame.render_stateful_widget(table, area, &mut state.sessions_table);
    draw_table_scrollbar(
        frame,
        area,
        count,
        &state.sessions_table,
        state.focus == Focus::Sessions,
    );
}

fn draw_notes(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.notes_table.selected();
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
                Cell::from(n.target.clone()),
            ])
        })
        .collect();
    let count = rows.len();
    let name_col = fit_col(
        "Name",
        state.model.notes.iter().map(|n| n.name.as_str()),
        area,
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            name_col,
            Constraint::Fill(2),
            Constraint::Fill(1),
        ],
    )
    .header(header_row(["Id", "Name", "Value", "Target"]))
    .row_highlight_style(highlight_style(state.focus == Focus::Notes))
    .block(panel_block(
        format!(" Notes ({count}) "),
        state.focus == Focus::Notes,
    ));
    state.notes_viewport = table_viewport(area);
    frame.render_stateful_widget(table, area, &mut state.notes_table);
    draw_table_scrollbar(
        frame,
        area,
        count,
        &state.notes_table,
        state.focus == Focus::Notes,
    );
}

fn draw_issues(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let selected = state.issues_table.selected();
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
        .row_highlight_style(highlight_style(state.focus == Focus::Issues))
        .block(panel_block(
            format!(" Issues ({count}) "),
            state.focus == Focus::Issues,
        ));
    state.issues_viewport = table_viewport(area);
    frame.render_stateful_widget(table, area, &mut state.issues_table);
    draw_table_scrollbar(
        frame,
        area,
        count,
        &state.issues_table,
        state.focus == Focus::Issues,
    );
}

/// Vertical scrollbar on a table panel's right border, matching the
/// session viewer's treatment. The viewport excludes the panel borders
/// and the table header row.
fn draw_table_scrollbar(
    frame: &mut Frame,
    area: Rect,
    len: usize,
    table: &TableState,
    active: bool,
) {
    let viewport = area.height.saturating_sub(3) as usize;
    let max_offset = len.saturating_sub(viewport);
    let mut sb_state = ScrollbarState::new(max_offset).position(table.offset());
    frame.render_stateful_widget(
        scrollbar(active),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut sb_state,
    );
}

fn scrollbar(active: bool) -> Scrollbar<'static> {
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("┃")
        .track_symbol(Some("│"))
        .style(style::scrollbar(active))
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &ViewState) {
    let help = if state.model.finished {
        "scan complete · q quit · Tab cycle · ↑/↓ select"
    } else {
        "q quit · Tab cycle · ↑/↓ select"
    };
    let help_width = help.width() as u16;
    let [log_area, help_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(help_width),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Span::styled(help, style::footer())),
        help_area,
    );
    if let Some(last) = state.log.back() {
        frame.render_widget(
            Paragraph::new(Span::styled(last.clone(), style::text_dim())),
            log_area,
        );
    }
}

fn initial_selection(len: usize) -> TableState {
    let mut state = TableState::default();
    if len > 0 {
        state.select(Some(0));
    }
    state
}

fn highlight_style(active: bool) -> ratatui::style::Style {
    if active {
        style::selection()
    } else {
        style::selection_inactive()
    }
}

fn panel_block(title: String, active: bool) -> Block<'static> {
    let border = if active {
        style::focus_border()
    } else {
        style::panel_border(false)
    };
    Block::bordered().title(title).border_style(border)
}

fn header_row<const N: usize>(names: [&'static str; N]) -> Row<'static> {
    Row::new(names.map(|n| Cell::from(Span::styled(n, style::text_dim()))))
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
        Span::styled(short_id(id), style::text_dim())
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
