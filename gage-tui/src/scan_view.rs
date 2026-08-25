//! Scan view — renders a [`ScanModel`]: overall progress, a tasks
//! table (with each task's agent sessions as expandable child rows),
//! a sessions table, and a results table.
//!
//! Two entry points share the rendering and key handling. [`run`]
//! drives a live scan: the caller runs the scan, adapts runner events
//! into [`Event`]s, and periodically reconciles notes/issues from the
//! db into [`Event::Results`]; the view applies them to the model and
//! lingers for inspection after the scan finishes. [`view`] renders an
//! already-complete model (a historical scan loaded from the db) with
//! no event source.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gage_claude::review::review_command;
use gage_core::task::{task_display, task_name_display};
use gage_db::issue::{self, IssueStatus, StatusReason};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Paragraph, Row, Table, Widget, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use gage_db::rusqlite::Connection;

use crate::attrs::attr_lines;
use crate::dialog;
use crate::item_table::ItemTable;
use crate::picker::{self, PickColumn, PickItem, Picker, PickerAction};

/// Loader used by the open-scan dialog to rebuild the model for a
/// picked scan id. Absent for live-scan views, where `o` is disabled.
type ScanLoader<'a> = &'a dyn Fn(&str) -> io::Result<ScanModel>;
use crate::scroll::ScrollView;
use crate::session_view::{pop_keyboard_enhancements, push_keyboard_enhancements};
use crate::text::{ellipsize, fmt_duration};
use crate::textarea::TextArea;
use crate::{app, hint, markdown, session, styles};

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
    pub scan_id: String,
    pub tasks: Vec<TaskItem>,
    pub sessions: Vec<SessionItem>,
    pub notes: Vec<NoteItem>,
    pub issues: Vec<IssueItem>,
    pub total: usize,
    pub progress: usize,
    pub errors: usize,
    /// Recorded agent spend; None when the scan has no agent data yet.
    pub cost: Option<ScanCost>,
    pub finished: bool,
    /// Scan duration; None while a live scan is running.
    pub elapsed: Option<Duration>,
    /// The scan's captured stdout stream (`{scan_id}.out`), shown by
    /// the log dialog. None disables the dialog.
    pub out_path: Option<PathBuf>,
}

/// Agent spend in USD. `incomplete` marks a total that understates
/// true spend (an agent's cost is unrecorded) and renders as a
/// trailing `+`.
#[derive(Debug, Clone, Copy)]
pub struct ScanCost {
    pub usd: f64,
    pub incomplete: bool,
}

/// A task's recorded agent spend.
#[derive(Debug, Clone)]
pub struct TaskCost {
    pub id: TaskId,
    pub cost: ScanCost,
}

/// An agent session with its owning task, as delivered by a
/// [`Event::Results`] refresh.
#[derive(Debug, Clone)]
pub struct TaskAgent {
    pub task: TaskId,
    pub agent: AgentItem,
}

#[derive(Debug, Clone)]
pub struct TaskItem {
    pub id: TaskId,
    pub state: TaskState,
    /// Recorded agent spend; None when the task has no agent data.
    pub cost: Option<ScanCost>,
    pub elapsed: Option<Duration>,
    /// Live-scan dispatch time; drives the ticking elapsed display.
    pub started: Option<Instant>,
    /// Latest task-reported `(pos, total)` while running; None renders
    /// as indeterminate.
    pub progress: Option<(u64, u64)>,
    /// Agent sessions spawned by this task, in recorded order
    pub agents: Vec<AgentItem>,
}

/// One agent session spawned by a task.
#[derive(Debug, Clone)]
pub struct AgentItem {
    pub session_id: String,
    /// Archived JSONL source, read by the session dialog; None when
    /// the session is not on disk
    pub path: Option<PathBuf>,
    pub state: AgentState,
    /// Recorded spend in USD; None until a terminal result is recorded
    pub cost: Option<f64>,
    /// First session-entry timestamp, epoch milliseconds
    pub started_ms: Option<i64>,
    /// Last session-entry timestamp, epoch milliseconds; None while
    /// the agent is running
    pub ended_ms: Option<i64>,
}

/// Agent state derived from its `task_agent` row: a terminal result
/// gives Done or Error; a recorded exit without a result is Error; no
/// exit recorded is Running (rendered as failed once the scan is over,
/// since nothing can finish it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Done,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Completed,
    Error,
    Skipped,
    Canceled,
}

impl TaskState {
    /// Sort rank: running, then pending, then finished
    fn rank(self) -> u8 {
        match self {
            TaskState::Running => 0,
            TaskState::Pending => 1,
            TaskState::Completed | TaskState::Error | TaskState::Skipped | TaskState::Canceled => 2,
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
    /// Raw JSON text for the note's metadata, if any.
    pub metadata: Option<String>,
}

/// An issue opened during the scan.
#[derive(Debug, Clone)]
pub struct IssueItem {
    pub id: String,
    pub name: String,
    pub title: String,
    /// Status display string, e.g. `open` or `closed (completed)`
    pub status: String,
    /// Compact status for the issues table: the close reason when one
    /// is recorded (a reason implies closed), otherwise the status
    pub status_cell: String,
    pub closed: bool,
    pub author: String,
    /// Creation time display string
    pub created: String,
    pub description: Option<String>,
    /// Sessions the issue applies to, per its session links
    pub sessions: Vec<IssueSessionItem>,
    pub evidence: Vec<EvidenceItem>,
    pub events: Vec<EventItem>,
}

/// A session an issue applies to, resolved for display.
#[derive(Debug, Clone)]
pub struct IssueSessionItem {
    pub id: String,
    /// Project display path, `~`-substituted when under HOME
    pub project: String,
    /// Session title
    pub title: String,
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
                cost: None,
                started: None,
                elapsed: None,
                progress: None,
                agents: Vec::new(),
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
                    let entry = running.iter().find(|r| r.id == item.id);
                    match item.state {
                        TaskState::Pending if entry.is_some() => {
                            item.state = TaskState::Running;
                            item.started = Some(Instant::now());
                        }
                        // No explicit completion event yet: a task that
                        // leaves the worker set without a Failed event
                        // is inferred completed.
                        TaskState::Running if entry.is_none() => {
                            item.state = TaskState::Completed;
                            item.elapsed = item.started.map(|t| t.elapsed());
                            item.progress = None;
                        }
                        _ => {}
                    }
                    if let Some(entry) = entry {
                        item.progress = entry.progress;
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
                cost,
                task_costs,
                agents,
            } => {
                self.notes = notes.clone();
                self.issues = issues.clone();
                self.cost = *cost;
                for tc in task_costs {
                    if let Some(item) = self.tasks.iter_mut().find(|t| t.id == tc.id) {
                        item.cost = Some(tc.cost);
                    }
                }
                for item in &mut self.tasks {
                    item.agents.clear();
                }
                for ta in agents {
                    if let Some(item) = self.tasks.iter_mut().find(|t| t.id == ta.task) {
                        item.agents.push(ta.agent.clone());
                    }
                }
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

/// A task currently assigned to a worker, with any task-reported
/// progress.
#[derive(Debug, Clone)]
pub struct RunningTask {
    pub id: TaskId,
    pub progress: Option<(u64, u64)>,
}

/// Events the view consumes while a live scan runs.
#[derive(Debug, Clone)]
pub enum Event {
    /// Self-contained progress snapshot: task totals plus the set of
    /// tasks currently assigned to workers.
    Status {
        total: usize,
        progress: usize,
        running: Vec<RunningTask>,
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
        cost: Option<ScanCost>,
        task_costs: Vec<TaskCost>,
        agents: Vec<TaskAgent>,
    },
    /// The scan is over; the view stays up until the user quits.
    Finished,
}

/// Show a live scan, applying events to the model as they arrive.
/// `on_cancel` requests an orderly scan cancellation; the view calls
/// it when the user confirms canceling mid-scan, then waits for
/// [`Event::Finished`] (or channel close) as confirmation that the
/// scan has stopped.
pub async fn run(
    model: ScanModel,
    mut events: UnboundedReceiver<Event>,
    on_cancel: impl Fn(),
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = event_loop(&mut terminal, model, &mut events, &on_cancel, None).await;
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result
}

/// Show an already-complete model (a historical scan). `None` opens
/// the scan picker first (canceling it exits); `load` rebuilds the
/// model when the user opens a different scan (`o`, or the picker).
pub async fn view(
    model: Option<ScanModel>,
    load: impl Fn(&str) -> io::Result<ScanModel>,
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = view_inner(&mut terminal, model, &load).await;
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result
}

async fn view_inner(
    terminal: &mut DefaultTerminal,
    model: Option<ScanModel>,
    load: ScanLoader<'_>,
) -> io::Result<()> {
    let mut model = match model {
        Some(m) => m,
        None => match standalone_scan_pick(terminal, load)? {
            Some(m) => m,
            None => return Ok(()),
        },
    };
    model.finished = true;
    let (tx, mut events) = unbounded_channel();
    drop(tx);
    // A finished model never opens the cancel path
    event_loop(terminal, model, &mut events, &|| {}, Some(load)).await
}

/// Run the open dialog on a blank background until the user picks a
/// scan or cancels.
fn standalone_scan_pick(
    terminal: &mut DefaultTerminal,
    load: ScanLoader<'_>,
) -> io::Result<Option<ScanModel>> {
    let mut picker = scan_picker(None)?;
    // Empty panels behind the picker, so picking renders in place
    // with no flash.
    let mut shell = ViewState::new(ScanModel::default());
    loop {
        terminal.draw(|frame| {
            draw(frame, &mut shell);
            picker.draw(frame);
        })?;
        if let TermEvent::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match picker.handle_key(key.code) {
                PickerAction::None => {}
                PickerAction::Close => return Ok(None),
                PickerAction::Open(id) => return load(&id).map(Some),
            }
        }
    }
}

/// Build the scan-open picker from the db's scans, newest first.
fn scan_picker(current: Option<&str>) -> io::Result<Picker> {
    let conn = gage_db::db::open_db().map_err(io::Error::other)?;
    let mut scans = gage_db::scan::all(&conn).map_err(io::Error::other)?;
    let counts = gage_db::scan::counts_by_scan(&conn).map_err(io::Error::other)?;
    scans.sort_by_key(|s| std::cmp::Reverse(s.created));
    let items = scans
        .into_iter()
        .map(|scan| {
            let metadata = scan.parse_metadata();
            let status = match &metadata {
                Ok(Some(gage_db::scan::ScanMetadata::Scan(s))) if s.canceled => "canceled",
                Ok(Some(gage_db::scan::ScanMetadata::Scan(_)))
                | Ok(Some(gage_db::scan::ScanMetadata::Agent(_))) => "completed",
                Ok(Some(gage_db::scan::ScanMetadata::Running(_))) => "running",
                // NULL metadata (a run that died before summarizing) and
                // unparseable metadata read the same: incomplete.
                Ok(None) | Err(_) => "incomplete",
            };
            let duration = match metadata {
                Ok(Some(m)) => m
                    .elapsed_ms()
                    .map(|ms| fmt_duration(Duration::from_millis(ms)))
                    .unwrap_or_default(),
                Ok(None) | Err(_) => String::new(),
            };
            let count = counts.get(&scan.id).copied().unwrap_or_default();
            let short = gage_core::uuid::short_uuid(&scan.id).to_string();
            PickItem {
                cells: vec![
                    Span::styled(short, styles::Text::id()),
                    Span::raw(count.tasks.to_string()),
                    Span::raw(count.sessions.to_string()),
                    Span::raw(status),
                    Span::raw(duration),
                    Span::styled(picker::ago(scan.created), styles::Text::dim()),
                ],
                id: scan.id,
            }
        })
        .collect();
    let columns = vec![
        PickColumn::new("Id", 8),
        PickColumn::right("Tasks", 5),
        PickColumn::right("Sessions", 8),
        PickColumn::new("Status", 10),
        PickColumn::right("Duration", 8),
        PickColumn::right("Created", 7),
    ];
    Ok(Picker::new("Open scan", columns, items, current))
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    model: ScanModel,
    events: &mut UnboundedReceiver<Event>,
    on_cancel: &impl Fn(),
    loader: Option<ScanLoader<'_>>,
) -> io::Result<()> {
    let mut state = ViewState::new(model);
    let mut stop_input = Arc::new(AtomicBool::new(false));
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
                    Some(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                        if handle_key(&mut state, key, on_cancel, loader) {
                            break;
                        }
                        if state.pending_review.is_some() {
                            run_review_session(terminal, &mut state, &mut input, &mut stop_input)
                                .await?;
                        }
                    }
                    // Input thread died (terminal input unavailable);
                    // without keys the view can never be backed, so
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

/// Hand the terminal to an interactive `/gage:review` session for the
/// staged issue, then restore the view. The input thread is stopped
/// first so the child owns stdin, and restarted with a fresh stop flag
/// once the session ends. The issue is re-read afterwards — the
/// session may have closed or commented it.
async fn run_review_session(
    terminal: &mut DefaultTerminal,
    state: &mut ViewState,
    input: &mut UnboundedReceiver<TermEvent>,
    stop_input: &mut Arc<AtomicBool>,
) -> io::Result<()> {
    let Some(issue_id) = state.pending_review.take() else {
        return Ok(());
    };
    stop_input.store(true, Ordering::Relaxed);
    // The channel closes when the input thread exits; draining until
    // then keeps buffered keys out of the child's session.
    while input.recv().await.is_some() {}
    pop_keyboard_enhancements();
    ratatui::restore();
    let result =
        review_command(std::slice::from_ref(&issue_id), None, &[]).and_then(|mut cmd| cmd.status());
    *terminal = ratatui::init();
    push_keyboard_enhancements();
    terminal.clear()?;
    *stop_input = Arc::new(AtomicBool::new(false));
    *input = spawn_input_thread(Arc::clone(stop_input));
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => state.push_log(format!(
            "Review session exited with status {}",
            status.code().unwrap_or(1)
        )),
        Err(e) => state.push_log(format!("Review session failed: {e}")),
    }
    state.refresh_issue(&issue_id);
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
fn handle_key(
    state: &mut ViewState,
    key: KeyEvent,
    on_cancel: &impl Fn(),
    loader: Option<ScanLoader<'_>>,
) -> bool {
    if state.prompt.is_some() {
        handle_prompt_key(state, key);
        return false;
    }
    match &state.dialog {
        Dialog::ConfirmQuit => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    // The scan can finish while the confirm is up; the
                    // affirmed cancel then reduces to a plain quit
                    if state.model.finished {
                        return true;
                    }
                    state.cancel_requested = true;
                    state.dialog = Dialog::Canceling;
                    on_cancel();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                    state.dialog = Dialog::None;
                }
                _ => {}
            }
            return false;
        }
        // No key handling while the cancellation completes; Finished
        // (or the events channel closing) dismisses the dialog
        Dialog::Canceling => return false,
        Dialog::ScanDone => {
            match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                    state.dialog = Dialog::None;
                }
                KeyCode::Char('l') => state.open_log(),
                _ => {}
            }
            return false;
        }
        Dialog::Session { .. } => {
            state.handle_session_dialog_key(key);
            return false;
        }
        Dialog::OpenScan(_) => {
            state.handle_open_scan_key(key.code, loader);
            return false;
        }
        Dialog::Notice { .. } => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q'))
                && let Dialog::Notice { return_to, .. } =
                    std::mem::replace(&mut state.dialog, Dialog::None)
            {
                state.dialog = *return_to;
            }
            return false;
        }
        Dialog::Note { .. } | Dialog::Issue { .. } | Dialog::Log { .. } => {
            let page = state.scroll_view.page() as isize;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => state.dialog = Dialog::None,
                KeyCode::Char('a') if matches!(state.dialog, Dialog::Issue { .. }) => {
                    state.open_actions_prompt();
                }
                KeyCode::Char('t')
                    if matches!(state.dialog, Dialog::Note { .. } | Dialog::Issue { .. }) =>
                {
                    state.open_tool_use_session();
                }
                KeyCode::Char('l') if matches!(state.dialog, Dialog::Log { .. }) => {
                    state.dialog = Dialog::None
                }
                KeyCode::Down | KeyCode::Char('j') => state.scroll_view.scroll_by(1),
                KeyCode::Up | KeyCode::Char('k') => state.scroll_view.scroll_by(-1),
                KeyCode::PageDown => state.scroll_view.scroll_by(page),
                KeyCode::PageUp => state.scroll_view.scroll_by(-page),
                KeyCode::Char('g') => state.scroll_view.scroll_to_top(),
                KeyCode::Char('G') => state.scroll_view.scroll_to_bottom(),
                KeyCode::Char(']') => state.step_dialog_item(1),
                KeyCode::Char('[') => state.step_dialog_item(-1),
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
        KeyCode::Char('a') if state.focus == Focus::Issues => state.actions_for_selected_issue(),
        KeyCode::Enter if state.focus == Focus::Sessions => state.open_selected_session(),
        KeyCode::Enter if state.focus == Focus::Tasks => state.enter_task_row(),
        KeyCode::Right if state.focus == Focus::Tasks => state.expand_task_row(),
        KeyCode::Left if state.focus == Focus::Tasks => state.collapse_task_row(),
        KeyCode::Char('l') => state.open_log(),
        KeyCode::Char('o') if state.model.finished && loader.is_some() => {
            match scan_picker(Some(&state.model.scan_id)) {
                Ok(picker) => state.dialog = Dialog::OpenScan(picker),
                Err(e) => state.push_log(format!("Open scan: {e}")),
            }
        }
        _ => {}
    }
    false
}

/// Key handling while a prompt overlays the view. `q`/Esc dismiss the
/// overlay, leaving whatever is beneath it untouched.
fn handle_prompt_key(state: &mut ViewState, key: KeyEvent) {
    match &state.prompt {
        Some(Prompt::Actions { issue }) => {
            let closed = issue.closed;
            match key.code {
                KeyCode::Char('r') => state.start_review(),
                KeyCode::Char('c') if !closed => state.start_close(StatusReason::Completed),
                KeyCode::Char('s') if !closed => state.start_close(StatusReason::Skipped),
                KeyCode::Char('d') if !closed => state.start_close(StatusReason::Duplicate),
                KeyCode::Char('o') if closed => state.start_open(IssueStatus::Open),
                KeyCode::Char('p') if closed => state.start_open(IssueStatus::Pending),
                KeyCode::Char('t') => state.start_comment(),
                KeyCode::Char('q') | KeyCode::Esc => state.prompt = None,
                _ => {}
            }
        }
        Some(Prompt::Close { .. }) | Some(Prompt::Open { .. }) | Some(Prompt::Comment { .. }) => {
            handle_comment_key(state, key)
        }
        Some(Prompt::Review { .. }) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => state.confirm_review(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                state.back_to_actions();
            }
            _ => {}
        },
        None => {}
    }
}

/// Comment entry for a close, reopen, or comment action. Enter applies
/// the change; Esc steps back to the actions menu. Newline shortcuts
/// mirror the note editor: terminals without the Kitty keyboard
/// protocol can't distinguish Shift+Enter from Enter, so Alt+Enter and
/// Ctrl+J are accepted as fallbacks.
fn handle_comment_key(state: &mut ViewState, key: KeyEvent) {
    let mods = key.modifiers;
    let is_newline_shortcut = (matches!(key.code, KeyCode::Enter)
        && (mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT)))
        || (matches!(key.code, KeyCode::Char('j')) && mods.contains(KeyModifiers::CONTROL));
    match key.code {
        KeyCode::Enter if !is_newline_shortcut => match &state.prompt {
            Some(Prompt::Close { .. }) => state.apply_close(),
            Some(Prompt::Open { .. }) => state.apply_open(),
            Some(Prompt::Comment { .. }) => state.apply_comment(),
            _ => {}
        },
        KeyCode::Esc => state.back_to_actions(),
        _ => {
            let editor = match &mut state.prompt {
                Some(Prompt::Close { editor, .. })
                | Some(Prompt::Open { editor, .. })
                | Some(Prompt::Comment { editor, .. }) => editor,
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

/// An issue's re-read status fields and history, applied onto
/// [`IssueItem`] snapshots after an external change.
struct IssueStatusUpdate {
    status: String,
    status_cell: String,
    closed: bool,
    events: Vec<EventItem>,
}

impl IssueStatusUpdate {
    fn apply(&self, item: &mut IssueItem) {
        item.status = self.status.clone();
        item.status_cell = self.status_cell.clone();
        item.closed = self.closed;
        item.events = self.events.clone();
    }
}

/// Re-read an issue's status fields and history from the db,
/// mirroring how the scan model builds them.
fn load_issue_status(issue_id: &str) -> Result<IssueStatusUpdate, String> {
    let conn = gage_db::db::open_db().map_err(|e| e.to_string())?;
    let issue = issue::get(&conn, issue_id).map_err(|e| e.to_string())?;
    let events = issue::issue_events_for(&conn, issue_id)
        .map_err(|e| e.to_string())?
        .iter()
        .map(|ev| EventItem {
            kind: ev.event.to_label(),
            author: ev.author.clone(),
            timestamp: gage_core::datetime::ms_to_iso8601(ev.timestamp),
            message: ev.event.message().map(str::to_string),
        })
        .collect();
    Ok(IssueStatusUpdate {
        status: match issue.status_reason {
            Some(r) => format!("{} ({})", issue.status.as_str(), r.as_str()),
            None => issue.status.as_str().to_string(),
        },
        status_cell: match issue.status_reason {
            Some(r) => r.as_str().to_string(),
            None => issue.status.as_str().to_string(),
        },
        closed: issue.status == IssueStatus::Closed,
        events,
    })
}

/// Set `issue_id`'s status in the db. A fresh connection per change
/// keeps the view free of a long-lived handle it rarely needs.
fn set_issue_status(
    issue_id: &str,
    status: IssueStatus,
    reason: Option<StatusReason>,
    author: &str,
    message: Option<&str>,
) -> Result<(), String> {
    let conn = gage_db::db::open_db().map_err(|e| e.to_string())?;
    issue::set_status(&conn, issue_id, status, reason, author, message).map_err(|e| e.to_string())
}

/// Record a comment against `issue_id` in the db. A fresh connection
/// per change, like [`set_issue_status`].
fn add_issue_comment(issue_id: &str, author: &str, message: &str) -> Result<(), String> {
    let conn = gage_db::db::open_db().map_err(|e| e.to_string())?;
    issue::comment(&conn, issue_id, author, message).map_err(|e| e.to_string())
}

/// Writer identity for issue events, matching the CLI's `user:{name}`.
fn user_author() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    format!("user:{user}")
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
fn fit_col<'a>(header: &str, values: impl Iterator<Item = &'a str>, area: Rect) -> u16 {
    let widest = values
        .map(|v| v.width())
        .max()
        .unwrap_or(0)
        .max(header.width());
    let cap = (area.width / 3) as usize;
    widest.min(cap) as u16
}

/// Rendered width of a table's single fill column: the panel interior
/// (borders excluded) minus the fixed column widths and one column gap
/// per fixed column.
fn fill_width(area: Rect, fixed: &[u16]) -> usize {
    let inner = area.width.saturating_sub(2) as usize;
    let fixed_total: usize = fixed.iter().map(|w| *w as usize).sum::<usize>() + fixed.len();
    inner.saturating_sub(fixed_total)
}

const LOG_CAP: usize = 500;

struct ViewState {
    model: ScanModel,
    focus: Focus,
    tasks: ItemTable,
    /// Task keys whose agent rows are shown. Stored as the expanded
    /// set so tasks surface collapsed by default.
    expanded: HashSet<String>,
    sessions: ItemTable,
    notes: ItemTable,
    issues: ItemTable,
    log: VecDeque<String>,
    started: Instant,
    dialog: Dialog,
    /// Confirm/comment prompt overlaying the dialog (or the bare view)
    prompt: Option<Prompt>,
    /// A live scan finished while another dialog was open; ScanDone
    /// shows once that dialog closes
    scan_done_pending: bool,
    /// The user confirmed canceling the scan; titles the ScanDone
    /// announcement as canceled when the cancellation completes
    cancel_requested: bool,
    /// Scroll state and layout cache for the open content dialog
    scroll_view: ScrollView,
    /// Issue whose confirmed review session the event loop should
    /// launch. Set by the review dialog's `y`; the loop takes it after
    /// key handling, since only the loop owns the terminal and input
    /// thread the launch must suspend.
    pending_review: Option<String>,
    /// Last UI position per viewed session, restored when a session
    /// dialog re-opens (including `[`/`]` stepping away and back).
    session_ui: HashMap<String, app::SavedUi>,
}

enum Dialog {
    None,
    /// Quit requested mid-scan; y cancels the scan
    ConfirmQuit,
    /// Cancel confirmed; waiting for the runner to wind down. Ignores
    /// all keys and closes itself when the scan is verified stopped.
    Canceling,
    /// Zoomed note detail. Holds a snapshot of the item so a results
    /// refresh can't shift what's being read.
    Note {
        note: NoteItem,
    },
    /// Zoomed issue detail; a snapshot, like Note
    Issue {
        issue: Box<IssueItem>,
    },
    /// Zoomed session view — the same tree/body component
    /// `gage session view` runs, embedded in a modal. `nav` selects
    /// what `[`/`]` steps through — the sessions table
    /// or the tasks panel's agent rows — and titles the dialog
    /// accordingly. The connection serves the view's note operations
    /// for the dialog's lifetime. `return_to` layers the dialog over
    /// another one (the note/issue dialog a tool-use jump came from):
    /// closing restores it instead of the bare view.
    Session {
        id: String,
        view: Box<app::AppState>,
        nav: SessionNav,
        db: Connection,
        return_to: Option<Box<Dialog>>,
    },
    /// Pure notification over another dialog; Enter restores it
    Notice {
        message: String,
        return_to: Box<Dialog>,
    },
    /// Captured scan streams (`{scan_id}.{err,out,log}`), reloaded
    /// from the files while a live scan runs.
    Log {
        content: Vec<Line<'static>>,
        loaded: Instant,
    },
    /// A live scan just finished
    ScanDone,
    /// Scan-open picker (`o`), historical views only
    OpenScan(Picker),
}

/// A confirm/comment prompt drawn over whatever is beneath it — the
/// issue dialog or the bare view, depending on where it was opened
/// from. Opening one never disturbs the open dialog or its scroll
/// state; dismissing removes only the overlay. Each variant holds a
/// snapshot of its issue so the prompt works with or without the
/// issue dialog beneath.
enum Prompt {
    /// Menu of the actions available for an issue, keyed by letter.
    /// Review and comment are always offered; the close reasons show
    /// for an open issue and the reopen statuses for a closed one.
    /// Every other prompt is entered from here, and backing out of one
    /// returns to this menu.
    Actions { issue: Box<IssueItem> },
    /// Close an issue with the reason picked in the actions menu:
    /// enter an optional comment and apply
    Close {
        issue: Box<IssueItem>,
        reason: StatusReason,
        editor: TextArea,
    },
    /// Confirm launching an interactive Claude Code review session for
    /// an issue. `y` suspends the view and hands the terminal to
    /// claude; the view resumes when the session ends.
    Review { issue: Box<IssueItem> },
    /// Reopen a closed issue with the status picked in the actions
    /// menu; the counterpart of `Close`
    Open {
        issue: Box<IssueItem>,
        status: IssueStatus,
        editor: TextArea,
    },
    /// Record a comment against an issue. Unlike the status prompts'
    /// comments, the text is required; Enter with an empty editor does
    /// nothing.
    Comment {
        issue: Box<IssueItem>,
        editor: TextArea,
    },
}

/// What a session dialog was opened from, and therefore what `[`/`]`
/// steps through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionNav {
    /// The sessions table
    Sessions,
    /// The tasks panel's agent rows
    Agents,
    /// A single agent session opened by a tool-use jump; stepping is
    /// disabled
    Pinned,
}

impl ViewState {
    fn new(model: ScanModel) -> Self {
        let mut state = Self {
            tasks: ItemTable::new(),
            expanded: HashSet::new(),
            sessions: ItemTable::new(),
            notes: ItemTable::new(),
            issues: ItemTable::new(),
            focus: Focus::Tasks,
            log: VecDeque::new(),
            started: Instant::now(),
            dialog: Dialog::None,
            prompt: None,
            scan_done_pending: false,
            cancel_requested: false,
            scroll_view: ScrollView::new(),
            pending_review: None,
            session_ui: HashMap::new(),
            model,
        };
        state.sync_tables();
        state
    }

    /// Swap in a different scan's model (historical only), resetting
    /// the view.
    fn replace_model(&mut self, mut model: ScanModel) {
        model.finished = true;
        *self = ViewState::new(model);
    }

    /// Route a key to the scan-open picker; Enter loads the picked
    /// scan through `loader` and swaps the model in.
    fn handle_open_scan_key(&mut self, code: KeyCode, loader: Option<ScanLoader<'_>>) {
        let action = match &mut self.dialog {
            Dialog::OpenScan(picker) => picker.handle_key(code),
            _ => return,
        };
        match action {
            PickerAction::None => {}
            PickerAction::Close => self.dialog = Dialog::None,
            PickerAction::Open(id) => {
                self.dialog = Dialog::None;
                if id != self.model.scan_id
                    && let Some(load) = loader
                {
                    match load(&id) {
                        Ok(model) => self.replace_model(model),
                        Err(e) => self.push_log(format!("Open scan: {e}")),
                    }
                }
            }
        }
    }

    /// Reconcile every table's selection with the model after a
    /// change — data replacement or re-sort moves rows under the
    /// positional table state; the tables re-anchor by item id.
    fn sync_tables(&mut self) {
        let task_ids = flat_task_ids(&self.model, &self.expanded);
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

    /// Open the session view over the agent session holding the
    /// tool-use entry named by the open note/issue dialog's author,
    /// layered so closing it restores the dialog. When the entry
    /// cannot be located, a notice overlays the dialog instead.
    fn open_tool_use_session(&mut self) {
        let (author, kind) = match &self.dialog {
            Dialog::Note { note } => (note.author.clone(), "note"),
            Dialog::Issue { issue } => (issue.author.clone(), "issue"),
            _ => return,
        };
        let Some(call_id) = author_call_id(&author) else {
            return;
        };
        let hit = self.find_tool_use_entry(call_id);
        let return_to = Box::new(std::mem::replace(&mut self.dialog, Dialog::None));
        self.dialog = match hit {
            Some(hit) => {
                let options = crate::ViewOptions {
                    show_turns: true,
                    ..Default::default()
                };
                let mut view = app::AppState::new(hit.doc, &options, hit.source);
                view.select_entry(hit.entry_index);
                Dialog::Session {
                    id: hit.item.id,
                    view: Box::new(view),
                    nav: SessionNav::Pinned,
                    db: hit.db,
                    return_to: Some(return_to),
                }
            }
            None => Dialog::Notice {
                message: format!("Cannot find tool use entry for {kind} author."),
                return_to,
            },
        };
    }

    /// Search the scan's agent sessions for the entry making tool-use
    /// call `call_id`. Agent JSONL files are text-scanned first so only
    /// candidate sessions are parsed; a file that cannot be read falls
    /// through to the full document load, which surfaces the error.
    /// Load errors go to the scan log and the search continues.
    fn find_tool_use_entry(&mut self, call_id: &str) -> Option<ToolUseHit> {
        let items: Vec<SessionItem> = self
            .model
            .tasks
            .iter()
            .flat_map(|t| t.agents.iter().map(|a| agent_session_item(t, a)))
            .collect();
        let db = match gage_db::db::open_db() {
            Ok(db) => db,
            Err(e) => {
                self.push_log(format!("Find tool use {call_id}: {e}"));
                return None;
            }
        };
        for item in items {
            if let Some(path) = &item.path
                && let Ok(text) = std::fs::read_to_string(path)
                && !text.contains(call_id)
            {
                continue;
            }
            match load_session_doc(&item, &db) {
                Ok((doc, source)) => {
                    if let Some(entry_index) = tool_use_entry_index(&doc, call_id) {
                        return Some(ToolUseHit {
                            item,
                            doc,
                            source,
                            entry_index,
                            db,
                        });
                    }
                }
                Err(e) => self.push_log(format!("Open session {}: {e}", item.id)),
            }
        }
        None
    }

    /// Open the actions menu over the issue dialog's issue
    fn open_actions_prompt(&mut self) {
        if let Dialog::Issue { issue } = &self.dialog {
            self.prompt = Some(Prompt::Actions {
                issue: issue.clone(),
            });
        }
    }

    /// Open the actions menu for the issues table's selected issue
    fn actions_for_selected_issue(&mut self) {
        let Some(i) = self.issues.selected_index() else {
            return;
        };
        if let Some(issue) = self.model.issues.get(i) {
            self.prompt = Some(Prompt::Actions {
                issue: Box::new(issue.clone()),
            });
        }
    }

    /// Take the actions menu's issue to hand to a chosen action's
    /// prompt. None (leaving the prompt untouched) when the open
    /// prompt is not the actions menu.
    fn take_actions_issue(&mut self) -> Option<Box<IssueItem>> {
        match self.prompt.take() {
            Some(Prompt::Actions { issue }) => Some(issue),
            other => {
                self.prompt = other;
                None
            }
        }
    }

    fn start_review(&mut self) {
        if let Some(issue) = self.take_actions_issue() {
            self.prompt = Some(Prompt::Review { issue });
        }
    }

    fn start_close(&mut self, reason: StatusReason) {
        if let Some(issue) = self.take_actions_issue() {
            self.prompt = Some(Prompt::Close {
                issue,
                reason,
                editor: TextArea::new(""),
            });
        }
    }

    fn start_open(&mut self, status: IssueStatus) {
        if let Some(issue) = self.take_actions_issue() {
            self.prompt = Some(Prompt::Open {
                issue,
                status,
                editor: TextArea::new(""),
            });
        }
    }

    fn start_comment(&mut self) {
        if let Some(issue) = self.take_actions_issue() {
            self.prompt = Some(Prompt::Comment {
                issue,
                editor: TextArea::new(""),
            });
        }
    }

    /// Step back from an action's prompt to the actions menu. Entered
    /// comment text is discarded.
    fn back_to_actions(&mut self) {
        let issue = match self.prompt.take() {
            Some(Prompt::Close { issue, .. })
            | Some(Prompt::Open { issue, .. })
            | Some(Prompt::Comment { issue, .. })
            | Some(Prompt::Review { issue }) => issue,
            other => {
                self.prompt = other;
                return;
            }
        };
        self.prompt = Some(Prompt::Actions { issue });
    }

    /// Apply the close prompt's reason and comment to the db, reflect
    /// the change in the results table and any open issue dialog, and
    /// dismiss the prompt. On a db failure the prompt stays up with
    /// its state intact and the error goes to the scan log.
    fn apply_close(&mut self) {
        let Some(Prompt::Close {
            mut issue,
            reason,
            editor,
        }) = self.prompt.take()
        else {
            return;
        };
        let text = editor.text();
        let trimmed = text.trim();
        let message = (!trimmed.is_empty()).then(|| trimmed.to_string());
        let author = user_author();
        if let Err(e) = set_issue_status(
            &issue.id,
            IssueStatus::Closed,
            Some(reason),
            &author,
            message.as_deref(),
        ) {
            self.push_log(format!("Close issue {} failed: {e}", issue.id));
            self.prompt = Some(Prompt::Close {
                issue,
                reason,
                editor,
            });
            return;
        }
        issue.status = format!("closed ({})", reason.as_str());
        issue.status_cell = reason.as_str().to_string();
        issue.closed = true;
        issue.events.push(EventItem {
            kind: format!("close ({})", reason.as_str()),
            author,
            timestamp: gage_core::datetime::ms_to_iso8601(gage_core::datetime::now_ms()),
            message,
        });
        self.reflect_issue(&issue);
        self.push_log(format!("Closed issue {} ({})", issue.id, reason.as_str()));
    }

    /// Reflect a changed issue snapshot in the results table and in an
    /// open issue dialog showing it. The dialog's rendered content is
    /// rebuilt at its current scroll position.
    fn reflect_issue(&mut self, issue: &IssueItem) {
        if let Some(item) = self.model.issues.iter_mut().find(|i| i.id == issue.id) {
            *item = issue.clone();
        }
        if let Dialog::Issue { issue: open } = &mut self.dialog
            && open.id == issue.id
        {
            **open = issue.clone();
            self.scroll_view.invalidate();
        }
    }

    /// Reopen the prompt's issue with the chosen status and comment,
    /// reflect the change, and dismiss the prompt. On a db failure the
    /// prompt stays up with its state intact and the error goes to the
    /// scan log.
    fn apply_open(&mut self) {
        let Some(Prompt::Open {
            mut issue,
            status,
            editor,
        }) = self.prompt.take()
        else {
            return;
        };
        let text = editor.text();
        let trimmed = text.trim();
        let message = (!trimmed.is_empty()).then(|| trimmed.to_string());
        let author = user_author();
        if let Err(e) = set_issue_status(&issue.id, status, None, &author, message.as_deref()) {
            self.push_log(format!("Open issue {} failed: {e}", issue.id));
            self.prompt = Some(Prompt::Open {
                issue,
                status,
                editor,
            });
            return;
        }
        issue.status = status.as_str().to_string();
        issue.status_cell = status.as_str().to_string();
        issue.closed = false;
        issue.events.push(EventItem {
            kind: status.as_str().to_string(),
            author,
            timestamp: gage_core::datetime::ms_to_iso8601(gage_core::datetime::now_ms()),
            message,
        });
        self.reflect_issue(&issue);
        self.push_log(format!("Opened issue {} ({})", issue.id, status.as_str()));
    }

    /// Record the comment prompt's text against the issue, reflect the
    /// new event, and dismiss the prompt. The comment is required:
    /// Enter with an empty editor keeps the prompt up. On a db failure
    /// the prompt stays up with its text intact and the error goes to
    /// the scan log.
    fn apply_comment(&mut self) {
        let Some(Prompt::Comment { mut issue, editor }) = self.prompt.take() else {
            return;
        };
        let text = editor.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.prompt = Some(Prompt::Comment { issue, editor });
            return;
        }
        let author = user_author();
        if let Err(e) = add_issue_comment(&issue.id, &author, trimmed) {
            self.push_log(format!("Comment on issue {} failed: {e}", issue.id));
            self.prompt = Some(Prompt::Comment { issue, editor });
            return;
        }
        issue.events.push(EventItem {
            kind: "comment".to_string(),
            author,
            timestamp: gage_core::datetime::ms_to_iso8601(gage_core::datetime::now_ms()),
            message: Some(trimmed.to_string()),
        });
        self.reflect_issue(&issue);
        self.push_log(format!("Commented on issue {}", issue.id));
    }

    /// Confirm the review: stage the issue for the event loop's launch
    /// and dismiss the prompt.
    fn confirm_review(&mut self) {
        let Some(Prompt::Review { issue }) = self.prompt.take() else {
            return;
        };
        self.pending_review = Some(issue.id.clone());
    }

    /// Re-read an issue's status and history from the db after the
    /// review session, which may have changed either. Updates the
    /// results table and any open issue dialog; a read failure keeps
    /// the stale snapshot and goes to the scan log.
    fn refresh_issue(&mut self, issue_id: &str) {
        let loaded = match load_issue_status(issue_id) {
            Ok(loaded) => loaded,
            Err(e) => {
                self.push_log(format!("Refresh issue {issue_id} failed: {e}"));
                return;
            }
        };
        if let Some(item) = self.model.issues.iter_mut().find(|i| i.id == issue_id) {
            loaded.apply(item);
        }
        if let Dialog::Issue { issue } = &mut self.dialog
            && issue.id == issue_id
        {
            loaded.apply(issue);
            self.scroll_view.invalidate();
        }
    }

    /// Move the source table's selection while an item dialog is open
    /// and reload the dialog with the newly selected item. The log
    /// dialog has no backing table and is unaffected.
    fn step_dialog_item(&mut self, delta: isize) {
        match self.dialog {
            Dialog::Note { .. } => {
                let ids: Vec<&str> = self.model.notes.iter().map(|n| n.id.as_str()).collect();
                let before = self.notes.selected_index();
                self.notes.select_by(delta, &ids);
                if self.notes.selected_index() != before {
                    self.open_selected_note();
                }
            }
            Dialog::Issue { .. } => {
                let ids: Vec<&str> = self.model.issues.iter().map(|i| i.id.as_str()).collect();
                let before = self.issues.selected_index();
                self.issues.select_by(delta, &ids);
                if self.issues.selected_index() != before {
                    self.open_selected_issue();
                }
            }
            Dialog::Session { nav, .. } => match nav {
                SessionNav::Sessions => {
                    let ids: Vec<&str> =
                        self.model.sessions.iter().map(|s| s.id.as_str()).collect();
                    let before = self.sessions.selected_index();
                    self.sessions.select_by(delta, &ids);
                    if self.sessions.selected_index() != before {
                        self.open_selected_session();
                    }
                }
                SessionNav::Agents => self.step_agent_dialog(delta),
                SessionNav::Pinned => {}
            },
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
        let session = session.clone();
        self.open_session_dialog(&session, SessionNav::Sessions);
    }

    /// Open the shared session view over `item` in a modal. The view is
    /// the same component `gage session view` runs; its UI position is
    /// restored when the session was viewed before.
    fn open_session_dialog(&mut self, item: &SessionItem, nav: SessionNav) {
        let db = match gage_db::db::open_db() {
            Ok(db) => db,
            Err(e) => {
                self.push_log(format!("Open session {}: {e}", item.id));
                return;
            }
        };
        let (doc, source) = match load_session_doc(item, &db) {
            Ok(loaded) => loaded,
            Err(e) => {
                self.push_log(format!("Open session {}: {e}", item.id));
                return;
            }
        };
        let options = crate::ViewOptions {
            show_turns: true,
            ..Default::default()
        };
        let mut view = app::AppState::new(doc, &options, source);
        if let Some(saved) = self.session_ui.get(&item.id) {
            view.restore_ui(saved);
        }
        self.dialog = Dialog::Session {
            id: item.id.clone(),
            view: Box::new(view),
            nav,
            db,
            return_to: None,
        };
    }

    /// Route a key to the embedded session view, then apply what the
    /// outcome means at this level: `Close` dismisses the dialog
    /// (restoring the dialog beneath, when layered) and `[`/`]`
    /// (`Ignored` by the view) steps to the neighboring session,
    /// remembering the current one's UI position.
    fn handle_session_dialog_key(&mut self, key: KeyEvent) {
        let mut close = false;
        let mut step: isize = 0;
        let mut error: Option<String> = None;
        if let Dialog::Session { id, view, db, .. } = &mut self.dialog {
            match app::handle_key(view, key, db) {
                Ok(app::KeyOutcome::Consumed) => {}
                Ok(app::KeyOutcome::Close) => {
                    self.session_ui.insert(id.clone(), view.save_ui());
                    close = true;
                }
                Ok(app::KeyOutcome::Ignored) => {
                    step = match key.code {
                        KeyCode::Char('[') => -1,
                        KeyCode::Char(']') => 1,
                        _ => 0,
                    };
                    if step != 0 {
                        self.session_ui.insert(id.clone(), view.save_ui());
                    }
                }
                Err(e) => error = Some(format!("Session view: {e}")),
            }
        }
        if let Some(msg) = error {
            self.push_log(msg);
        }
        if close {
            self.dialog = match std::mem::replace(&mut self.dialog, Dialog::None) {
                Dialog::Session {
                    return_to: Some(back),
                    ..
                } => *back,
                _ => Dialog::None,
            };
        } else if step != 0 {
            self.step_dialog_item(step);
        }
    }

    /// Enter on the tasks panel: a task row with agents toggles its
    /// expansion; an agent row opens its session dialog.
    fn enter_task_row(&mut self) {
        match self.selected_task_row() {
            Some(TaskRowAction::Toggle(key)) => {
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
                self.sync_tables();
            }
            Some(TaskRowAction::OpenAgent(item)) => self.open_agent_session(&item),
            None => {}
        }
    }

    /// Right on the tasks panel: expand a collapsed task row.
    fn expand_task_row(&mut self) {
        if let Some(TaskRowAction::Toggle(key)) = self.selected_task_row()
            && self.expanded.insert(key)
        {
            self.sync_tables();
        }
    }

    /// Left on the tasks panel: collapse an expanded task row; on an
    /// agent row, select the owning task first (the session viewer's
    /// collapse convention).
    fn collapse_task_row(&mut self) {
        let Some(i) = self.tasks.selected_index() else {
            return;
        };
        let target = {
            let rows = flat_task_rows(&self.model, &self.expanded);
            match rows.get(i) {
                Some(TaskRow::Task(t)) if !t.agents.is_empty() => {
                    CollapseTarget::Task(task_key(&t.id))
                }
                Some(TaskRow::Agent(t, _)) => CollapseTarget::Parent(task_key(&t.id)),
                _ => return,
            }
        };
        match target {
            CollapseTarget::Task(key) => {
                if self.expanded.remove(&key) {
                    self.sync_tables();
                }
            }
            CollapseTarget::Parent(parent) => {
                let ids = flat_task_ids(&self.model, &self.expanded);
                let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                if let Some(p) = refs.iter().position(|id| *id == parent) {
                    self.tasks.select_by(p as isize - i as isize, &refs);
                }
            }
        }
    }

    /// The action Enter would take on the selected tasks-panel row.
    fn selected_task_row(&self) -> Option<TaskRowAction> {
        let i = self.tasks.selected_index()?;
        match flat_task_rows(&self.model, &self.expanded).get(i)? {
            TaskRow::Task(t) if !t.agents.is_empty() => {
                Some(TaskRowAction::Toggle(task_key(&t.id)))
            }
            TaskRow::Agent(t, a) => Some(TaskRowAction::OpenAgent(agent_session_item(t, a))),
            TaskRow::Task(_) => None,
        }
    }

    fn open_agent_session(&mut self, item: &SessionItem) {
        self.open_session_dialog(item, SessionNav::Agents);
    }

    /// Step the session dialog to the previous/next agent session in
    /// task-list order, across every task regardless of expansion.
    /// The target's task is expanded when collapsed so the panel
    /// selection can follow the dialog.
    fn step_agent_dialog(&mut self, delta: isize) {
        let Dialog::Session { id, .. } = &self.dialog else {
            return;
        };
        let current_id = id.clone();
        let stepped = {
            let mut agents: Vec<(&TaskItem, &AgentItem)> = Vec::new();
            for t in self.model.sorted_tasks() {
                for a in &t.agents {
                    agents.push((t, a));
                }
            }
            let pos = agents.iter().position(|(_, a)| a.session_id == current_id);
            pos.and_then(|pos| {
                let next = (pos as isize + delta).clamp(0, agents.len() as isize - 1) as usize;
                if next == pos {
                    return None;
                }
                let (t, a) = *agents.get(next).expect("next is clamped in bounds");
                Some((task_key(&t.id), agent_session_item(t, a)))
            })
        };
        let Some((parent_key, item)) = stepped else {
            return;
        };
        if self.expanded.insert(parent_key) {
            self.sync_tables();
        }
        let ids = flat_task_ids(&self.model, &self.expanded);
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        if let (Some(cur), Some(target)) = (
            self.tasks.selected_index(),
            refs.iter().position(|id| *id == item.id),
        ) {
            self.tasks.select_by(target as isize - cur as isize, &refs);
        }
        self.open_agent_session(&item);
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
    /// then `.log`, one entry per line with a blank line between
    /// entries. Absent or empty files are skipped; the files are
    /// created lazily, so a scan may simply have produced nothing.
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
            for l in content.lines() {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                lines.push(match ext {
                    "err" => Line::from(Span::styled(l.to_string(), styles::LogLevel::error())),
                    "log" => log_line(l),
                    _ => Line::raw(l.to_string()),
                });
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("(no output)", styles::Text::dim())));
        }
        lines
    }

    /// Quitting a finished view is immediate; `q` mid-scan cancels
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
    ///
    /// A user-canceled scan announces with the same summary dialog,
    /// titled as canceled, replacing the Canceling dialog. Tasks left
    /// running or pending move to Canceled — the same terminal state
    /// the runner records for their db rows.
    fn mark_finished(&mut self) {
        if self.model.finished {
            return;
        }
        self.model.finished = true;
        self.model.elapsed = Some(self.started.elapsed());
        if self.cancel_requested {
            for item in &mut self.model.tasks {
                if matches!(item.state, TaskState::Running | TaskState::Pending) {
                    item.state = TaskState::Canceled;
                    item.elapsed = item.started.map(|t| t.elapsed());
                    item.progress = None;
                }
            }
            // The state changes re-rank the task sort; re-anchor the
            // table selections to the new order
            self.sync_tables();
            // Canceling ignores all keys, so it is still the open
            // dialog here
            self.dialog = Dialog::ScanDone;
            return;
        }
        if matches!(self.dialog, Dialog::None) && self.prompt.is_none() {
            self.dialog = Dialog::ScanDone;
        } else {
            self.scan_done_pending = true;
        }
    }

    /// Show a deferred ScanDone once no other dialog or prompt is
    /// open. Run each loop iteration before drawing.
    fn promote_pending_dialog(&mut self) {
        if self.scan_done_pending && matches!(self.dialog, Dialog::None) && self.prompt.is_none() {
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
            } => self.push_log(format!(
                "warning: {}: {message}",
                task_display(&scanner, &task)
            )),
            Event::Failed {
                scanner,
                task,
                message,
            } => {
                self.push_log(format!("error: {}", task_display(&scanner, &task)));
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
            (Focus::Tasks, true) | (Focus::Issues, false) => Focus::Sessions,
            (Focus::Sessions, true) | (Focus::Notes, false) => Focus::Issues,
            (Focus::Issues, true) | (Focus::Tasks, false) => Focus::Notes,
            (Focus::Notes, true) | (Focus::Sessions, false) => Focus::Tasks,
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
                let ids = flat_task_ids(&self.model, &self.expanded);
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

/// A task's row id — tasks have no single id column, so identity is
/// the `{scanner}::{task}` pair. Agent rows use the session id, which
/// cannot collide with this form.
fn task_key(id: &TaskId) -> String {
    format!("{}::{}", id.scanner, id.task)
}

/// One row of the tasks panel: a task, or an agent session under its
/// (expanded) task.
enum TaskRow<'a> {
    Task(&'a TaskItem),
    Agent(&'a TaskItem, &'a AgentItem),
}

/// The tasks panel's visible rows: tasks in display order, each
/// followed by its agent rows when expanded.
fn flat_task_rows<'a>(model: &'a ScanModel, expanded: &HashSet<String>) -> Vec<TaskRow<'a>> {
    let mut rows = Vec::new();
    for t in model.sorted_tasks() {
        rows.push(TaskRow::Task(t));
        if expanded.contains(&task_key(&t.id)) {
            for a in &t.agents {
                rows.push(TaskRow::Agent(t, a));
            }
        }
    }
    rows
}

/// Row ids for the tasks panel in visible order — the identity list
/// [`ItemTable`] anchors its selection to.
fn flat_task_ids(model: &ScanModel, expanded: &HashSet<String>) -> Vec<String> {
    flat_task_rows(model, expanded)
        .iter()
        .map(|row| match row {
            TaskRow::Task(t) => task_key(&t.id),
            TaskRow::Agent(_, a) => a.session_id.clone(),
        })
        .collect()
}

/// What Enter does on a tasks-panel row.
enum TaskRowAction {
    /// Toggle a task row's agent expansion
    Toggle(String),
    /// Open an agent row's session dialog
    OpenAgent(SessionItem),
}

/// What Left does on a tasks-panel row.
enum CollapseTarget {
    /// Collapse this task's agent rows
    Task(String),
    /// Select the agent row's owning task
    Parent(String),
}

/// The session dialog's view of an agent session. Titled by the
/// owning task; note/issue counts are zero — scanner notes and issues
/// target scanned sessions, not the agent sessions that wrote them.
fn agent_session_item(task: &TaskItem, agent: &AgentItem) -> SessionItem {
    SessionItem {
        id: agent.session_id.clone(),
        title: task_display(&task.id.scanner, &task.id.task),
        path: agent.path.clone(),
        notes: 0,
        issues: 0,
    }
}

/// A located tool-use entry: the agent session it was found in, the
/// session's loaded document, and the entry's position in it. The
/// connection is handed on to the session dialog.
struct ToolUseHit {
    item: SessionItem,
    doc: crate::doc::Document,
    source: app::DocSource,
    entry_index: usize,
    db: Connection,
}

/// The tool-use call id embedded in an author of the form
/// `{name}?call=toolu_{rest}`.
fn author_call_id(author: &str) -> Option<&str> {
    let (_, call) = author.split_once("?call=")?;
    call.starts_with("toolu_").then_some(call)
}

/// Index of the entry whose message content holds the tool_use block
/// with id `call_id`.
fn tool_use_entry_index(doc: &crate::doc::Document, call_id: &str) -> Option<usize> {
    use serde_json::Value;
    doc.entries.iter().position(|e| {
        e.message()
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_use")
                        && b.get("id").and_then(Value::as_str) == Some(call_id)
                })
            })
    })
}

/// An agent's duration: last minus first session timestamp once both
/// are known, otherwise first timestamp to now (a live agent still
/// writing). None without a start, or when the scan is over and no
/// end was ever recorded (nothing is still running to measure).
fn agent_duration(agent: &AgentItem, finished: bool) -> Option<Duration> {
    let started = agent.started_ms?;
    let ended = match agent.ended_ms {
        Some(e) => e,
        None if finished => return None,
        None => now_ms(),
    };
    Some(Duration::from_millis(
        ended.saturating_sub(started).max(0) as u64
    ))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be past the epoch")
        .as_millis() as i64
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [progress, tasks, sessions, issues, notes, footer] = Layout::vertical([
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
    draw_issues(frame, issues, state);
    draw_notes(frame, notes, state);
    draw_footer(frame, footer, state);
    draw_dialogs(frame, state);
}

fn draw_dialogs(frame: &mut Frame, state: &mut ViewState) {
    // Split borrows: the dialog holds the content, the scroll view
    // holds position and layout cache
    let ViewState {
        dialog,
        prompt,
        scroll_view,
        model,
        cancel_requested,
        ..
    } = state;
    match dialog {
        Dialog::ConfirmQuit => draw_confirm_quit(frame),
        Dialog::Canceling => dialog::draw_message(frame, "Canceling the scan...", ""),
        Dialog::Note { note } => {
            scroll_view.render_modal(frame, format!(" Note {} ", note.id), |_| {
                vec![note_lines(note)]
            })
        }
        Dialog::Issue { issue } => {
            scroll_view.render_modal(frame, format!(" Issue {} ", issue.id), |width| {
                vec![issue_lines(issue, width as usize, true)]
            })
        }
        Dialog::Session { id, view, nav, .. } => {
            let title = match nav {
                SessionNav::Sessions => format!(" Session {id} "),
                SessionNav::Agents | SessionNav::Pinned => format!(" Agent {id} "),
            };
            draw_session_dialog(frame, title, view);
        }
        Dialog::Notice { message, .. } => dialog::draw_message(frame, message, "Enter dismiss"),
        Dialog::Log { content, .. } => {
            scroll_view.render_modal(frame, " Log ".to_string(), |_| vec![content.clone()]);
        }
        Dialog::ScanDone => draw_scan_done(frame, model, *cancel_requested),
        Dialog::OpenScan(picker) => picker.draw(frame),
        Dialog::None => {}
    }
    if let Some(prompt) = prompt {
        draw_prompt(frame, prompt);
    }
}

/// Draw the open prompt over the frame. The prompt dialogs clear only
/// their own centered area, so whatever is beneath stays visible.
fn draw_prompt(frame: &mut Frame, prompt: &mut Prompt) {
    match prompt {
        Prompt::Actions { issue } => draw_actions(frame, issue),
        Prompt::Close { reason, editor, .. } => {
            let comment_prompt = match reason {
                StatusReason::Duplicate => "Comment — name the surviving issue ID",
                _ => "Comment (optional)",
            };
            draw_status_comment(
                frame,
                format!(" Close issue ({}) ", reason.as_str()),
                comment_prompt,
                "Enter close · Shift-Enter newline · Esc cancel",
                editor,
            );
        }
        Prompt::Review { .. } => dialog::draw_wrapped(
            frame,
            &[
                "You are about to start a review session in Claude Code. \
                 When finished, exit the session to return here.",
                "Start Claude Code?",
            ],
            "y / n",
        ),
        Prompt::Open { status, editor, .. } => draw_status_comment(
            frame,
            format!(" Open issue ({}) ", status.as_str()),
            "Comment (optional)",
            "Enter open · Shift-Enter newline · Esc cancel",
            editor,
        ),
        Prompt::Comment { editor, .. } => draw_status_comment(
            frame,
            " Comment issue ".to_string(),
            "Comment",
            "Enter comment · Shift-Enter newline · Esc cancel",
            editor,
        ),
    }
}

/// Session dialog chrome: the same inset modal the scroll view uses
/// for content dialogs, with the shared session view drawn inside.
fn draw_session_dialog(frame: &mut Frame, title: String, view: &mut app::AppState) {
    let area = frame.area();
    let margin_x = (area.width / 10).clamp(1, 4);
    let margin_y = (area.height / 10).clamp(1, 3);
    let rect = area.inner(ratatui::layout::Margin {
        horizontal: margin_x,
        vertical: margin_y,
    });
    frame.render_widget(Clear, rect);
    let block = Block::bordered().title(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    app::draw_in(frame, inner, view);
}

/// The actions menu: one left-aligned row per available action. Close
/// reasons show for an open issue, reopen statuses for a closed one.
fn draw_actions(frame: &mut Frame, issue: &IssueItem) {
    let mut actions = vec![("r", "review (starts interactive agent)")];
    if issue.closed {
        actions.push(("o", "reopen"));
        actions.push(("p", "set as pending"));
    } else {
        actions.push(("c", "close as completed"));
        actions.push(("s", "close as skipped"));
        actions.push(("d", "close as duplicate"));
    }
    actions.push(("t", "comment"));
    let lines = actions
        .into_iter()
        .map(|(key, label)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(key, styles::Dialog::key()),
                Span::raw(format!(" {label}")),
            ])
            .left_aligned()
        })
        .collect();
    dialog::draw_lines(frame, lines, "q cancel");
}

/// Note detail content: caption/value header columns (with an optional
/// multi-line Metadata attribute) and the value rendered as markdown.
fn note_lines(note: &NoteItem) -> Vec<Line<'static>> {
    let metadata_pretty = note.metadata.as_deref().map(pretty_json);
    let mut attrs: Vec<(&'static str, &str)> = vec![
        ("Name", &note.name),
        ("Target", &note.target),
        ("Author", &note.author),
        ("Created", &note.created),
    ];
    if let Some(pretty) = &metadata_pretty {
        attrs.push(("Metadata", pretty));
    }
    let mut lines = attr_lines(&attrs);
    lines.push(Line::raw(""));
    lines.extend(markdown::render(&note.value_full));
    lines
}

/// Pretty-print a raw JSON string (2-space indent). Falls back to the
/// raw text if the string does not parse as JSON.
fn pretty_json(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| raw.to_string())
}

/// Issue detail content, mirroring `gage issue show`. `related` adds
/// the Notes and Issue history sections; the session dialog omits them
/// to keep its embedded issues compact.
fn issue_lines(issue: &IssueItem, width: usize, related: bool) -> Vec<Line<'static>> {
    let mut lines = attr_lines(&[
        ("Name", &issue.name),
        ("Status", &issue.status),
        ("Author", &issue.author),
        ("Created", &issue.created),
    ]);
    let mut body = vec![Line::raw(issue.title.clone())];
    if let Some(description) = &issue.description {
        body.push(Line::raw(""));
        body.extend(markdown::render(description));
    }
    if related {
        if !issue.sessions.is_empty() {
            bar_section(
                &mut lines,
                "Target",
                issue_session_lines(&issue.sessions, width),
                width,
            );
        }
        bar_section(&mut lines, "Description", body, width);
    } else {
        lines.push(Line::raw(""));
        lines.extend(body);
        return lines;
    }

    if !issue.evidence.is_empty() {
        let mut content: Vec<Line<'static>> = Vec::new();
        for ev in &issue.evidence {
            let mut entry =
                attr_lines(&[("Id", &ev.id), ("Name", &ev.name), ("Target", &ev.target)]);
            entry.push(Line::raw(""));
            entry.extend(
                ev.value
                    .lines()
                    .map(|l| Line::from(Span::styled(l.to_string(), styles::Text::accent()))),
            );
            content.extend(content_box(entry, styles::Text::dim(), width));
        }
        bar_section(&mut lines, "Notes", content, width);
    }

    if !issue.events.is_empty() {
        let mut content: Vec<Line<'static>> = Vec::new();
        for ev in &issue.events {
            let mut entry = attr_lines(&[
                ("Change", &ev.kind),
                ("Author", &ev.author),
                ("Created", &ev.timestamp),
            ]);
            if let Some(message) = &ev.message {
                entry.push(Line::raw(""));
                entry.extend(
                    message
                        .lines()
                        .map(|l| Line::from(Span::styled(l.to_string(), styles::Text::accent()))),
                );
            }
            content.extend(content_box(entry, styles::Text::dim(), width));
        }
        bar_section(&mut lines, "Issue history", content, width);
    }

    lines
}

/// Two-column table of the sessions an issue applies to: dim column
/// headings like the panel tables, then one row per session. The
/// project column is content-fit and capped by `fit_col`, the session
/// title fills the rest, and both cells truncate so a row never wraps.
fn issue_session_lines(sessions: &[IssueSessionItem], width: usize) -> Vec<Line<'static>> {
    let area = Rect::new(0, 0, width as u16, 1);
    let project_width =
        fit_col("Project", sessions.iter().map(|s| s.project.as_str()), area) as usize;
    let title_width = width.saturating_sub(project_width + 2);
    let mut lines = vec![Line::from(Span::styled(
        format!("{:<title_width$}  Project", "Session"),
        styles::Text::dim(),
    ))];
    for s in sessions {
        lines.push(Line::from(vec![
            Span::raw(format!(
                "{:<title_width$}  ",
                ellipsize(&s.title, title_width)
            )),
            Span::raw(ellipsize(&s.project, project_width)),
        ]));
    }
    lines
}

/// Session dialog content: header attributes, message sections, the
/// session's issues, session-level notes (targets with no line
/// number), and an optional trailing notice (truncation or read
/// error).
/// Load the session document for the dialog's embedded view. Prefers
/// the indexed corpus (matching `gage session view`); a session absent
/// from the index — agent sessions, or one indexed as empty — falls
/// back to reading the JSONL directly when its path is known.
fn load_session_doc(
    item: &SessionItem,
    db: &Connection,
) -> Result<(crate::doc::Document, app::DocSource), String> {
    let indexed = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(session::load(&item.id, db))
    });
    match indexed {
        Ok(doc) if !doc.entries.is_empty() => Ok((doc, app::DocSource::Query)),
        other => match &item.path {
            Some(path) => {
                let doc = session::load_from_path(&item.id, path, db).map_err(|e| e.to_string())?;
                Ok((doc, app::DocSource::Path(path.clone())))
            }
            // No JSONL to fall back to: an empty indexed document still
            // renders (as empty); an index error is surfaced.
            None => match other {
                Ok(doc) => Ok((doc, app::DocSource::Query)),
                Err(e) => Err(e.to_string()),
            },
        },
    }
}

/// Content embedded in the session dialog — a note's or issue's
/// detail-dialog lines inside a full border, wrapped to fit the
/// dialog width.
fn content_box(content: Vec<Line<'static>>, border: Style, width: usize) -> Vec<Line<'static>> {
    // Border plus one column of inner padding each side
    let inner = width.saturating_sub(4);
    if inner == 0 {
        return Vec::new();
    }
    let rule = "─".repeat(width - 2);
    let mut out = vec![Line::from(Span::styled(format!("┌{rule}┐"), border))];
    for line in wrap_lines(content, inner as u16) {
        let pad = inner.saturating_sub(line.width());
        let mut spans = vec![Span::styled("│ ", border)];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(" │", border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(format!("└{rule}┘"), border)));
    out
}

/// Wrap styled lines to `width` columns. The scroll view leaves
/// wrapping to `Paragraph` at draw time, but bordered content must be
/// wrapped before the border is applied or long lines break the right
/// edge — so the same `Paragraph` wrapping runs here, into a scratch
/// buffer, and the wrapped rows are read back as owned lines.
fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = u16::try_from(paragraph.line_count(width)).unwrap_or(u16::MAX);
    let area = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut buf = Buffer::empty(area);
    paragraph.render(area, &mut buf);
    (0..height)
        .map(|y| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut x = 0;
            while x < width {
                let Some(cell) = buf.cell((x, y)) else {
                    break;
                };
                let symbol = cell.symbol();
                match spans.last_mut() {
                    Some(last) if last.style == cell.style() => {
                        last.content.to_mut().push_str(symbol);
                    }
                    _ => spans.push(Span::styled(symbol.to_string(), cell.style())),
                }
                // A wide grapheme owns its continuation cells; skip them
                x += symbol.width().max(1) as u16;
            }
            Line::from(spans)
        })
        .collect()
}

/// Header attributes as caption/value columns; the caption column pads
/// to the widest caption.
/// Section under a full-width header bar, matching the session
/// dialog's message headers: one column of inner padding, header
/// style across the width, a blank line before the content.
fn bar_section(
    lines: &mut Vec<Line<'static>>,
    caption: &'static str,
    content: Vec<Line<'static>>,
    width: usize,
) {
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!(" {caption:<pad$}", pad = width.saturating_sub(1)),
        styles::Text::header(),
    )));
    lines.push(Line::raw(""));
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
    dialog::draw_message(frame, "Cancel the scan? It cannot be restarted.", "y / n");
}

/// Comment entry for the close and reopen dialogs: a bordered editor
/// with the chosen status in the title and a key-hint footer, centered
/// like the message dialogs.
fn draw_status_comment(
    frame: &mut Frame,
    title: String,
    prompt: &'static str,
    hint: &'static str,
    editor: &mut TextArea,
) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(8).clamp(24, 72);
    let height = 10.min(frame_area.height.saturating_sub(2));
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Block::new().style(styles::Dialog::surface()), area);
    let [border_area, hint_row] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let block = Block::bordered().title(title);
    let inner = block.inner(border_area);
    frame.render_widget(block, border_area);
    let [prompt_area, editor_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled(prompt, styles::Dialog::dim())),
        prompt_area,
    );
    if let Some((x, y)) = editor.render(editor_area, frame.buffer_mut(), Style::default()) {
        frame.set_cursor_position((x, y));
    }
    frame.render_widget(
        Paragraph::new(Span::styled(hint, styles::Dialog::dim())).centered(),
        hint_row,
    );
}

fn draw_scan_done(frame: &mut Frame, model: &ScanModel, canceled: bool) {
    let title = match (canceled, model.elapsed) {
        (true, Some(e)) => format!("Scan canceled after {}", fmt_duration(e)),
        (true, None) => "Scan canceled".to_string(),
        (false, Some(e)) => format!("Scan completed in {}", fmt_duration(e)),
        (false, None) => "Scan completed".to_string(),
    };
    let mut lines = vec![
        Line::raw(format!("  {title}")).left_aligned(),
        Line::raw(""),
    ];
    if model.errors > 0 && model.out_path.is_some() {
        lines.push(
            Line::styled(
                "  There were errors during this scan. Press 'l'",
                styles::Dialog::error(),
            )
            .left_aligned(),
        );
        lines.push(Line::styled("  to view the log.", styles::Dialog::error()).left_aligned());
        lines.push(Line::raw(""));
    }
    let counts = [
        ("Sessions:", model.sessions.len()),
        ("Notes:", model.notes.len()),
        ("Issues:", model.issues.len()),
        ("Errors:", model.errors),
    ];
    let value_width = counts
        .iter()
        .map(|(_, n)| n.to_string().len())
        .max()
        .unwrap();
    for (label, n) in counts {
        lines.push(Line::raw(format!("  {label:<9} {n:>value_width$}")).left_aligned());
    }
    let hint = if model.out_path.is_some() {
        "Enter dismiss · l log"
    } else {
        "Enter dismiss"
    };
    dialog::draw_lines(frame, lines, hint);
}

fn draw_progress(frame: &mut Frame, area: Rect, state: &ViewState) {
    let model = &state.model;
    let mut count_spans = vec![
        Span::styled("  Issues ", styles::Text::dim()),
        Span::raw(model.issues.len().to_string()),
        Span::styled("  Notes ", styles::Text::dim()),
        Span::raw(model.notes.len().to_string()),
        Span::styled("  Errors ", styles::Text::dim()),
        if model.errors > 0 {
            Span::styled(model.errors.to_string(), styles::RunStatus::error())
        } else {
            Span::raw("0")
        },
    ];
    // No cost data reads as $0.00+ while running (spend not yet
    // recorded) and $0.00 once finished, so the entry never appears
    // or disappears mid-scan
    let (usd, incomplete) = match model.cost {
        Some(cost) => (cost.usd, cost.incomplete),
        None => (0.0, !model.finished),
    };
    let suffix = if incomplete { "+" } else { "" };
    count_spans.push(Span::styled("  Cost ", styles::Text::dim()));
    count_spans.push(Span::raw(format!("${usd:.2}{suffix}")));
    let counts = Line::from(count_spans);
    let [tasks, badges] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(counts.width() as u16),
    ])
    .areas(area);

    // Each task owns an equal 1/total share of the bar. A completed
    // task contributes its full share; a running task that reports
    // progress contributes the reported fraction of its share.
    let ratio = if model.total > 0 {
        let running: f64 = model
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .filter_map(|t| t.progress)
            .map(|(pos, total)| {
                if total > 0 {
                    (pos as f64 / total as f64).min(1.0)
                } else {
                    0.0
                }
            })
            .sum();
        ((model.progress as f64 + running) / model.total as f64).min(1.0)
    } else {
        0.0
    };
    if model.finished {
        // A finished model without a duration (a historical scan that
        // never completed) shows no time rather than a ticking one.
        let summary = match model.elapsed {
            Some(e) => format!(
                " · {}/{} tasks run in {}",
                model.progress,
                model.total,
                fmt_duration(e)
            ),
            None => format!(" · {}/{} tasks run", model.progress, model.total),
        };
        let line = Line::from(vec![
            Span::styled(model.scan_id.clone(), styles::Text::id()),
            Span::styled(summary, styles::Text::dim()),
        ]);
        frame.render_widget(Paragraph::new(line), tasks);
    } else {
        let label = format!(
            "{}/{} · {}",
            model.progress,
            model.total,
            fmt_duration(state.started.elapsed())
        );
        let id_width = model.scan_id.width() as u16;
        let [id_area, _gap, gauge_area] = Layout::horizontal([
            Constraint::Length(id_width),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(tasks);
        frame.render_widget(
            Paragraph::new(Span::styled(model.scan_id.clone(), styles::Text::id())),
            id_area,
        );
        frame.render_widget(
            Gauge::default()
                .gauge_style(styles::Panel::gauge())
                .ratio(ratio)
                .label(label),
            gauge_area,
        );
    }

    frame.render_widget(Paragraph::new(counts), badges);
}

/// Indicatif-style braille spinner; one frame per 100ms redraw tick
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn draw_tasks(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let frame_idx = (state.started.elapsed().as_millis() / 100) as usize % SPINNER.len();
    let spinner = *SPINNER.get(frame_idx).expect("mod len is in bounds");
    let selected = state.tasks.selected_index();
    let finished = state.model.finished;
    let flat = flat_task_rows(&state.model, &state.expanded);

    // Status cell contents are computed up front: the column sizes to
    // the widest `{label} {time}` value, and the progress fill needs
    // that final width before the spans are built.
    let labels: Vec<(&'static str, Style, String)> = flat
        .iter()
        .map(|row| match row {
            TaskRow::Task(t) => {
                let (label, style) = match t.state {
                    TaskState::Pending => ("pending", styles::RunStatus::pending()),
                    TaskState::Running => ("running", styles::RunStatus::running()),
                    TaskState::Completed => ("done", styles::RunStatus::completed()),
                    TaskState::Error => ("error", styles::RunStatus::error()),
                    TaskState::Skipped => ("skipped", styles::RunStatus::skipped()),
                    TaskState::Canceled => ("canceled", styles::RunStatus::skipped()),
                };
                let time = match t.state {
                    // Read a stable "0s" during the first second rather than
                    // showing the sub-second ramp up on each refresh
                    TaskState::Running => t
                        .started
                        .map(|s| {
                            let e = s.elapsed();
                            if e < Duration::from_secs(1) {
                                "0s".to_string()
                            } else {
                                fmt_duration(e)
                            }
                        })
                        .unwrap_or_default(),
                    _ => t.elapsed.map(fmt_duration).unwrap_or_default(),
                };
                (label, style, time)
            }
            TaskRow::Agent(t, a) => {
                let (label, style, _) = agent_status(a, t, finished);
                let time = agent_duration(a, finished)
                    .map(|e| {
                        // Same sub-second hold as running tasks
                        if a.ended_ms.is_none() && e < Duration::from_secs(1) {
                            "0s".to_string()
                        } else {
                            fmt_duration(e)
                        }
                    })
                    .unwrap_or_default();
                (label, style, time)
            }
        })
        .collect();
    // The label field sizes to the widest possible status label, not
    // the labels on screen, so the column doesn't jitter as states
    // change. One space each side: the label aligns under the padded
    // header and the time doesn't touch the panel border.
    let label_width = ["pending", "running", "done", "error", "skipped", "canceled"]
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap();
    // Time field floor: room for sub-hour durations ("59m59s") so the
    // column doesn't widen as times tick up; longer runs still grow it
    let min_time_width = "59m59s".width();
    let status_width = labels
        .iter()
        .map(|(_, _, time)| label_width + 1 + time.width().max(min_time_width))
        .max()
        .unwrap_or(label_width + 1 + min_time_width)
        .max("Status".width())
        + 2;

    let costs: Vec<String> = flat
        .iter()
        .map(|row| match row {
            TaskRow::Task(t) => match t.cost {
                Some(c) => {
                    let suffix = if c.incomplete { "+" } else { "" };
                    format!("${:.2}{suffix}", c.usd)
                }
                None => String::new(),
            },
            TaskRow::Agent(_, a) => match a.cost {
                Some(usd) => format!("${usd:.2}"),
                None => String::new(),
            },
        })
        .collect();
    let cost_width = costs
        .iter()
        .map(|c| c.width())
        .max()
        .unwrap_or(0)
        .max("Cost".width());

    let rows: Vec<Row> = flat
        .iter()
        .zip(labels.iter().zip(&costs))
        .enumerate()
        .map(|(i, (row, ((label, label_style, time), cost)))| {
            let is_selected = selected == Some(i);
            let pad = status_width - 2 - label.width();
            let text = format!(" {label}{time:>pad$} ");
            let progress = match row {
                TaskRow::Task(t) => t.progress,
                TaskRow::Agent(..) => None,
            };
            let status = if let Some((pos, total)) = progress {
                let ratio = if total > 0 {
                    (pos as f64 / total as f64).min(1.0)
                } else {
                    0.0
                };
                // The selected-row variant compensates for the REVERSED
                // highlight, which only the focused panel uses; under
                // the unfocused gray highlight the normal fill styling
                // composes correctly
                let reversed = is_selected && state.focus == Focus::Tasks;
                Cell::from(gauge_line(&text, ratio, status_width, reversed))
            } else if is_selected {
                // Colored/dim cells on the selected row would invert
                // into per-cell backgrounds under the REVERSED
                // highlight, so the label renders plain
                Cell::from(text)
            } else {
                Cell::from(Span::styled(text, *label_style))
            };
            let name = match row {
                TaskRow::Task(t) => {
                    // Fold arrow matching the session viewer's outline;
                    // blank for a task with no agent rows to expand
                    let fold = if t.agents.is_empty() {
                        "  "
                    } else if state.expanded.contains(&task_key(&t.id)) {
                        "▼ "
                    } else {
                        "▶ "
                    };
                    let glyph = match t.state {
                        TaskState::Pending => "□",
                        TaskState::Running => spinner,
                        TaskState::Completed => "✓",
                        TaskState::Error => "✗",
                        TaskState::Skipped | TaskState::Canceled => "⊘",
                    };
                    Cell::from(format!(
                        "{fold}{glyph} {}:{}",
                        t.id.scanner,
                        task_name_display(&t.id.task)
                    ))
                }
                TaskRow::Agent(t, a) => {
                    let (_, _, glyph) = agent_status(a, t, finished);
                    let id = short_id(&a.session_id);
                    let text = format!("    {glyph} {id}");
                    if is_selected {
                        Cell::from(text)
                    } else {
                        Cell::from(Span::styled(text, styles::Text::dim()))
                    }
                }
            };
            Row::new(vec![
                name,
                Cell::from(format!("{cost:>cost_width$}")),
                status,
            ])
        })
        .collect();
    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(cost_width as u16),
            Constraint::Length(status_width as u16),
        ],
    )
    .header(Row::new([
        // Left pad Task past the "<fold><glyph> " prefix on each value
        Cell::from(Span::styled("    Task", styles::Text::dim())),
        // Right-align Cost over its right-aligned values
        Cell::from(Span::styled(
            format!("{:>cost_width$}", "Cost"),
            styles::Text::dim(),
        )),
        // Left pad Status to align with values
        Cell::from(Span::styled(" Status", styles::Text::dim())),
    ]))
    .row_highlight_style(styles::Panel::selection(state.focus == Focus::Tasks))
    .block(panel_block(
        format!(" Tasks ({count}) "),
        state.focus == Focus::Tasks,
    ));
    state
        .tasks
        .render(frame, area, table, count, state.focus == Focus::Tasks);
}

/// An agent row's status label, style, and glyph. A non-terminal agent
/// after the scan is over can never finish — it renders canceled under
/// a canceled task, error otherwise.
fn agent_status(
    agent: &AgentItem,
    parent: &TaskItem,
    finished: bool,
) -> (&'static str, Style, &'static str) {
    match agent.state {
        AgentState::Done => ("done", styles::RunStatus::completed(), "✓"),
        AgentState::Error => ("error", styles::RunStatus::error(), "✗"),
        AgentState::Running if !finished => ("running", styles::RunStatus::running(), "•"),
        AgentState::Running if parent.state == TaskState::Canceled => {
            ("canceled", styles::RunStatus::skipped(), "⊘")
        }
        AgentState::Running => ("error", styles::RunStatus::error(), "✗"),
    }
}

/// The `Gauge` technique in a table cell: the leading `ratio` share of
/// the cell renders as fill (reverse-video gauge color) with the text
/// over it, the remainder as plain gauge color — the same look as the
/// overall progress bar's label.
///
/// On the selected row the highlight patches REVERSED onto every cell,
/// under which an explicitly-reversed fill is indistinguishable from
/// the rest of the row. There the filled share carries the plain gauge
/// color instead — the highlight inverts it into a visible fill — and
/// the remainder is left for the highlight alone.
fn gauge_line(text: &str, ratio: f64, width: usize, selected: bool) -> Line<'static> {
    let fill = (ratio * width as f64).round() as usize;
    let (head, rest) = split_at_width(text, fill);
    let (head_style, rest_style) = if selected {
        (styles::Panel::gauge(), Style::new())
    } else {
        (styles::Panel::gauge_fill(), styles::Panel::gauge())
    };
    Line::from(vec![
        Span::styled(head.to_string(), head_style),
        Span::styled(rest.to_string(), rest_style),
    ])
}

/// Split at a display-column offset. A wide grapheme straddling the
/// boundary stays in the tail.
fn split_at_width(s: &str, cols: usize) -> (&str, &str) {
    let mut w = 0;
    for (i, c) in s.char_indices() {
        let cw = c.width().unwrap_or(0);
        if w + cw > cols {
            return s.split_at(i);
        }
        w += cw;
    }
    (s, "")
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
    let widths: Vec<Constraint> = vec![
        Constraint::Length(8),
        Constraint::Fill(1),
        Constraint::Length(6),
        Constraint::Length(7),
    ];
    let header = header_row(["Id", "Title", "Notes", "Issues"]);
    let table = Table::new(rows, widths)
        .header(header)
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
    let value_width = fill_width(area, &[8, name_col, target_col]);
    let rows: Vec<Row> = state
        .model
        .notes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            Row::new(vec![
                Cell::from(id_span(&n.id, selected == Some(i))),
                Cell::from(n.name.clone()),
                Cell::from(ellipsize(&n.value, value_width)),
                Cell::from(n.target_cell.clone()),
            ])
        })
        .collect();
    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(name_col),
            Constraint::Fill(1),
            Constraint::Length(target_col),
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
    // Wide enough for one short id plus half of a second — a clue that
    // more sessions are linked without ceding width to the full list
    const SESSIONS_CAP: usize = 13;
    let session_cells: Vec<String> = state
        .model
        .issues
        .iter()
        .map(|i| {
            let ids: Vec<String> = i.sessions.iter().map(|s| short_id(&s.id)).collect();
            ids.join(" ")
        })
        .collect();
    let sessions_width = session_cells
        .iter()
        .map(|c| c.width())
        .max()
        .unwrap_or(0)
        .max("Sessions".width())
        .min(SESSIONS_CAP);
    let name_col = fit_col(
        "Name",
        state.model.issues.iter().map(|i| i.name.as_str()),
        area,
    );
    let status_col = fit_col(
        "Status",
        state.model.issues.iter().map(|i| i.status_cell.as_str()),
        area,
    );
    let title_width = fill_width(area, &[8, name_col, 5, sessions_width as u16, status_col]);
    let rows: Vec<Row> = state
        .model
        .issues
        .iter()
        .zip(&session_cells)
        .enumerate()
        .map(|(idx, (i, sessions))| {
            let notes = match i.evidence.len() {
                0 => String::new(),
                n => n.to_string(),
            };
            Row::new(vec![
                Cell::from(id_span(&i.id, selected == Some(idx))),
                Cell::from(i.name.clone()),
                Cell::from(ellipsize(&i.title, title_width)),
                Cell::from(notes),
                Cell::from(ellipsize(sessions, sessions_width)),
                Cell::from(i.status_cell.clone()),
            ])
        })
        .collect();
    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(name_col),
            Constraint::Fill(1),
            Constraint::Length(5),
            Constraint::Length(sessions_width as u16),
            Constraint::Length(status_col),
        ],
    )
    .header(header_row([
        "Id", "Name", "Title", "Notes", "Sessions", "Status",
    ]))
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
    let help = footer_help(state);
    if help.width() == 0 {
        return;
    }
    let help_width = help.width() as u16;
    let [_, help_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(help_width),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(help).style(styles::Panel::footer()),
        help_area,
    );
}

/// Help text for the keys active in the current dialog state. Dialogs
/// and prompts that show their key options inline (confirm, scan-done,
/// issue prompts) get an empty footer.
fn footer_help(state: &ViewState) -> Line<'static> {
    if state.prompt.is_some() {
        return Line::default();
    }
    match &state.dialog {
        Dialog::ConfirmQuit | Dialog::Canceling | Dialog::ScanDone | Dialog::Notice { .. } => {
            Line::default()
        }
        Dialog::Note { note } => {
            let mut items = vec![("q", "close"), ("↑/↓", "scroll"), ("[/]", "prev/next")];
            if author_call_id(&note.author).is_some() {
                items.push(("t", "tool use"));
            }
            hint::help_line(&items)
        }
        Dialog::Issue { issue } => {
            let mut items = vec![("q", "close"), ("↑/↓", "scroll"), ("[/]", "prev/next")];
            if author_call_id(&issue.author).is_some() {
                items.push(("t", "tool use"));
            }
            items.push(("a", "action"));
            hint::help_line(&items)
        }
        Dialog::Session { nav, .. } => {
            let mut items = vec![
                ("q", "close"),
                ("Tab", "pane"),
                ("j/k g/G", ""),
                ("n", "note"),
                ("r", "refresh"),
            ];
            if session_step_targets(state, *nav) > 1 {
                items.push(("[/]", "prev/next"));
            }
            hint::help_line(&items)
        }
        Dialog::Log { .. } => hint::help_line(&[("q", "close"), ("↑/↓", "scroll")]),
        Dialog::OpenScan(_) => Line::default(),
        Dialog::None if state.focus == Focus::Issues => {
            if state.model.finished {
                hint::help_line(&[
                    ("q", "quit"),
                    ("Tab", "cycle"),
                    ("↑/↓", "select"),
                    ("a", "action"),
                    ("l", "log"),
                    ("o", "open"),
                ])
            } else {
                hint::help_line(&[
                    ("q", "cancel scan"),
                    ("Tab", "cycle"),
                    ("↑/↓", "select"),
                    ("a", "action"),
                    ("l", "log"),
                ])
            }
        }
        Dialog::None if state.model.finished => hint::help_line(&[
            ("q", "quit"),
            ("Tab", "cycle"),
            ("↑/↓", "select"),
            ("l", "log"),
            ("o", "open"),
        ]),
        Dialog::None => hint::help_line(&[
            ("q", "cancel scan"),
            ("Tab", "cycle"),
            ("↑/↓", "select"),
            ("l", "log"),
        ]),
    }
}

/// Number of items `[`/`]` step through for a session dialog opened
/// from `nav` — the sessions table's rows, or the tasks panel's
/// visible agent rows. A pinned dialog steps through nothing but
/// itself.
fn session_step_targets(state: &ViewState, nav: SessionNav) -> usize {
    match nav {
        SessionNav::Sessions => state.model.sessions.len(),
        SessionNav::Agents => flat_task_rows(&state.model, &state.expanded)
            .iter()
            .filter(|row| matches!(row, TaskRow::Agent(..)))
            .count(),
        SessionNav::Pinned => 1,
    }
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
