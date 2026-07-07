//! Scan progress view — live TUI for `gage scan`.
//!
//! Renders overall progress, a tasks table, a sessions table, and a
//! results table while a scan runs, then lingers for inspection until
//! the user quits. The view is a pure event consumer: the caller runs
//! the scan and adapts runner events into [`Event`]s on the channel
//! passed to [`run`]. Results and note/issue counts have no runner
//! event source yet and render empty.

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
    Block, Cell, Gauge, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
    TableState,
};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::style;

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

/// Everything known before the scan starts.
#[derive(Debug, Clone)]
pub struct ScanSetup {
    pub tasks: Vec<TaskId>,
    pub sessions: Vec<SessionEntry>,
}

/// Events the view consumes while the scan runs.
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
    /// The scan is over; the view stays up until the user quits.
    Finished,
}

pub async fn run(setup: ScanSetup, mut events: UnboundedReceiver<Event>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, setup, &mut events).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    setup: ScanSetup,
    events: &mut UnboundedReceiver<Event>,
) -> io::Result<()> {
    let mut state = ViewState::new(setup);
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
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab => state.cycle_focus(1),
        KeyCode::BackTab => state.cycle_focus(-1),
        KeyCode::Down | KeyCode::Char('j') => state.select_by(1),
        KeyCode::Up | KeyCode::Char('k') => state.select_by(-1),
        _ => {}
    }
    false
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Focus {
    Tasks,
    Sessions,
    Results,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum TaskStatus {
    Pending,
    Running,
    Completed,
    Error,
}

impl TaskStatus {
    /// Sort rank: running first, then finished, then pending
    fn rank(self) -> u8 {
        match self {
            TaskStatus::Running => 0,
            TaskStatus::Completed | TaskStatus::Error => 1,
            TaskStatus::Pending => 2,
        }
    }
}

struct TaskRow {
    id: TaskId,
    status: TaskStatus,
    started: Option<Instant>,
    elapsed: Option<Duration>,
}

struct SessionRow {
    id: String,
    title: String,
    notes: usize,
    issues: usize,
}

struct ResultRow {
    kind: &'static str,
    id: String,
    text: String,
}

struct ViewState {
    tasks: Vec<TaskRow>,
    sessions: Vec<SessionRow>,
    results: Vec<ResultRow>,
    focus: Focus,
    tasks_table: TableState,
    sessions_table: TableState,
    results_table: TableState,
    total: usize,
    progress: usize,
    notes: usize,
    issues: usize,
    errors: usize,
    log: VecDeque<String>,
    started: Instant,
    finished: bool,
    /// Scan duration, frozen when the scan finishes
    scan_elapsed: Option<Duration>,
}

const LOG_CAP: usize = 500;

impl ViewState {
    fn new(setup: ScanSetup) -> Self {
        let tasks = setup
            .tasks
            .into_iter()
            .map(|id| TaskRow {
                id,
                status: TaskStatus::Pending,
                started: None,
                elapsed: None,
            })
            .collect::<Vec<_>>();
        let sessions: Vec<SessionRow> = setup
            .sessions
            .into_iter()
            .map(|s| SessionRow {
                id: s.id,
                title: s.title,
                notes: 0,
                issues: 0,
            })
            .collect();
        let total = tasks.len();
        Self {
            tasks_table: initial_selection(total),
            sessions_table: initial_selection(sessions.len()),
            results_table: TableState::default(),
            tasks,
            sessions,
            results: Vec::new(),
            focus: Focus::Tasks,
            total,
            progress: 0,
            notes: 0,
            issues: 0,
            errors: 0,
            log: VecDeque::new(),
            started: Instant::now(),
            finished: false,
            scan_elapsed: None,
        }
    }

    fn mark_finished(&mut self) {
        if !self.finished {
            self.finished = true;
            self.scan_elapsed = Some(self.started.elapsed());
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Status {
                total,
                progress,
                running,
            } => {
                self.total = total;
                self.progress = progress;
                for row in &mut self.tasks {
                    let is_running = running.contains(&row.id);
                    match row.status {
                        TaskStatus::Pending if is_running => {
                            row.status = TaskStatus::Running;
                            row.started = Some(Instant::now());
                        }
                        // No explicit completion event yet: a task that
                        // leaves the worker set without a Failed event
                        // is inferred completed.
                        TaskStatus::Running if !is_running => {
                            row.status = TaskStatus::Completed;
                            row.elapsed = row.started.map(|t| t.elapsed());
                        }
                        _ => {}
                    }
                }
            }
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
                self.errors += 1;
                if let Some(row) = self
                    .tasks
                    .iter_mut()
                    .find(|r| r.id.scanner == scanner && r.id.task == task)
                {
                    row.status = TaskStatus::Error;
                    row.elapsed = row.started.map(|t| t.elapsed());
                }
                self.push_log(format!("error: {scanner}::{task}"));
                for line in message.lines() {
                    self.push_log(format!("  {line}"));
                }
            }
            Event::Finished => self.mark_finished(),
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
            (Focus::Tasks, true) | (Focus::Results, false) => Focus::Sessions,
            (Focus::Sessions, true) | (Focus::Tasks, false) => Focus::Results,
            (Focus::Results, true) | (Focus::Sessions, false) => Focus::Tasks,
        };
    }

    fn select_by(&mut self, delta: isize) {
        let (table, len) = match self.focus {
            Focus::Tasks => (&mut self.tasks_table, self.tasks.len()),
            Focus::Sessions => (&mut self.sessions_table, self.sessions.len()),
            Focus::Results => (&mut self.results_table, self.results.len()),
        };
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

    /// Task rows in display order: running, finished, pending, each
    /// group sorted by name.
    fn sorted_tasks(&self) -> Vec<&TaskRow> {
        let mut rows: Vec<&TaskRow> = self.tasks.iter().collect();
        rows.sort_by(|a, b| {
            a.status
                .rank()
                .cmp(&b.status.rank())
                .then_with(|| a.id.scanner.cmp(&b.id.scanner))
                .then_with(|| a.id.task.cmp(&b.id.task))
        });
        rows
    }
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [progress, tasks, sessions, results, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(4),
        Constraint::Fill(3),
        Constraint::Fill(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_progress(frame, progress, state);
    draw_tasks(frame, tasks, state);
    draw_sessions(frame, sessions, state);
    draw_results(frame, results, state);
    draw_footer(frame, footer, state);
}

fn draw_progress(frame: &mut Frame, area: Rect, state: &ViewState) {
    let [tasks_label, tasks, badges] = Layout::horizontal([
        Constraint::Length(6),
        Constraint::Fill(1),
        Constraint::Length(30),
    ])
    .areas(area);

    frame.render_widget(Paragraph::new("Tasks "), tasks_label);
    let ratio = if state.total > 0 {
        (state.progress as f64 / state.total as f64).min(1.0)
    } else {
        0.0
    };
    let elapsed = fmt_secs(
        state
            .scan_elapsed
            .unwrap_or_else(|| state.started.elapsed()),
    );
    let label = if state.finished {
        format!("{}/{} · done {elapsed}", state.progress, state.total)
    } else {
        format!("{}/{} · {elapsed}", state.progress, state.total)
    };
    frame.render_widget(
        Gauge::default()
            .gauge_style(style::gauge())
            .ratio(ratio)
            .label(label),
        tasks,
    );

    let counts = Line::from(vec![
        Span::styled("  Notes ", style::text_dim()),
        Span::raw(state.notes.to_string()),
        Span::styled("  Issues ", style::text_dim()),
        Span::raw(state.issues.to_string()),
        Span::styled("  Errors ", style::text_dim()),
        if state.errors > 0 {
            Span::styled(state.errors.to_string(), style::error())
        } else {
            Span::raw("0")
        },
    ]);
    frame.render_widget(Paragraph::new(counts), badges);
}

fn draw_tasks(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let rows: Vec<Row> = state
        .sorted_tasks()
        .iter()
        .map(|t| {
            let (status, status_style) = match t.status {
                TaskStatus::Pending => ("pending", style::text_dim()),
                TaskStatus::Running => ("running", style::running()),
                TaskStatus::Completed => ("done", style::text_dim()),
                TaskStatus::Error => ("error", style::error()),
            };
            let time = match t.status {
                TaskStatus::Pending => String::new(),
                TaskStatus::Running => t.started.map(|s| fmt_secs(s.elapsed())).unwrap_or_default(),
                TaskStatus::Completed | TaskStatus::Error => {
                    t.elapsed.map(fmt_secs).unwrap_or_default()
                }
            };
            Row::new(vec![
                Cell::from(format!("{}::{}", t.id.scanner, t.id.task)),
                Cell::from(Span::styled(status, status_style)),
                Cell::from(time),
            ])
        })
        .collect();
    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(8),
            Constraint::Length(7),
        ],
    )
    .header(header_row(["Task", "Status", "Time"]))
    .row_highlight_style(highlight_style(state.focus == Focus::Tasks))
    .block(panel_block(
        format!(" Tasks ({count}) "),
        state.focus == Focus::Tasks,
    ));
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
    let rows: Vec<Row> = state
        .sessions
        .iter()
        .map(|s| {
            Row::new(vec![
                Cell::from(short_id(&s.id)),
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
    frame.render_stateful_widget(table, area, &mut state.sessions_table);
    draw_table_scrollbar(
        frame,
        area,
        count,
        &state.sessions_table,
        state.focus == Focus::Sessions,
    );
}

fn draw_results(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let block = panel_block(" Results ".to_string(), state.focus == Focus::Results);
    if state.results.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("No results yet", style::text_dim())).block(block),
            area,
        );
        return;
    }
    let rows: Vec<Row> = state
        .results
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.kind),
                Cell::from(short_id(&r.id)),
                Cell::from(r.text.clone()),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
    )
    .header(header_row(["Type", "Id", "Description"]))
    .row_highlight_style(highlight_style(state.focus == Focus::Results))
    .block(block);
    let count = state.results.len();
    frame.render_stateful_widget(table, area, &mut state.results_table);
    draw_table_scrollbar(
        frame,
        area,
        count,
        &state.results_table,
        state.focus == Focus::Results,
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
    let help = if state.finished {
        "scan complete · q quit · Tab cycle · ↑/↓ select"
    } else {
        "q quit · Tab cycle · ↑/↓ select"
    };
    let mut spans = vec![Span::styled(help, style::footer())];
    if let Some(last) = state.log.back() {
        spans.push(Span::styled("  │ ", style::footer()));
        spans.push(Span::styled(last.clone(), style::text_dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

fn fmt_secs(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}
