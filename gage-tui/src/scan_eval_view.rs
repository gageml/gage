//! Scan eval view — two panes: a report-style summary of the scan's
//! measurements, and the scan's agent sessions with triage columns.
//! Enter on a session opens the shared session viewer (with note
//! taking); ←/→ in the dialog steps to the neighboring session. The
//! caller builds a [`ScanEvalModel`] from the run's `scan.json`.

use std::io;
use std::path::PathBuf;

use gage_db::rusqlite::Connection;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, ScrollbarState, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};

use unicode_width::UnicodeWidthStr;

use crate::attrs::attr_lines;
use crate::item_table::{ItemTable, scrollbar};
use crate::session_view::{pop_keyboard_enhancements, push_keyboard_enhancements};
use crate::{app, session, styles};

pub struct ScanEvalModel {
    pub run_id: String,
    pub scan_id: String,
    /// Overview attributes, already formatted (label, value)
    pub attrs: Vec<(&'static str, String)>,
    /// Per scanner/task aggregate rows, preformatted
    pub scanner_lines: Vec<String>,
    /// Error clusters, preformatted, most frequent first
    pub cluster_lines: Vec<String>,
    /// Agent sessions, ordered by session id
    pub sessions: Vec<ScanSessionItem>,
}

pub struct ScanSessionItem {
    pub id: String,
    pub scanner: String,
    pub task: String,
    pub turns: Option<u32>,
    pub cost: Option<f64>,
    pub tool_errors: u32,
    /// Notes + issues the session wrote
    pub output: u32,
    /// Transcript JSONL; None when the file is gone
    pub path: Option<PathBuf>,
}

pub fn run(model: ScanEvalModel) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = event_loop(&mut terminal, model);
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, model: ScanEvalModel) -> io::Result<()> {
    let mut state = ViewState::new(model);
    loop {
        terminal.draw(|frame| draw(frame, &mut state))?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            state.error = None;
            if handle_key(&mut state, key) {
                return Ok(());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Summary,
    Sessions,
}

struct ViewState {
    model: ScanEvalModel,
    focus: Focus,
    sessions: ItemTable,
    summary_scroll: u16,
    summary_max_scroll: u16,
    summary_viewport: u16,
    dialog: Dialog,
    /// Last UI position per viewed session, restored on re-open
    session_ui: std::collections::HashMap<String, app::SavedUi>,
    /// Last operation error, shown in the footer until the next key
    error: Option<String>,
}

enum Dialog {
    None,
    /// An agent session in the shared session view component
    Session {
        index: usize,
        view: Box<app::AppState>,
        db: Connection,
    },
}

impl ViewState {
    fn new(model: ScanEvalModel) -> Self {
        let mut sessions = ItemTable::new();
        let ids: Vec<&str> = model.sessions.iter().map(|s| s.id.as_str()).collect();
        sessions.update(&ids);
        Self {
            model,
            focus: Focus::Summary,
            sessions,
            summary_scroll: 0,
            summary_max_scroll: 0,
            summary_viewport: 0,
            dialog: Dialog::None,
            session_ui: std::collections::HashMap::new(),
            error: None,
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Summary => Focus::Sessions,
            Focus::Sessions => Focus::Summary,
        };
    }

    fn summary_scroll_by(&mut self, delta: i32) {
        let next =
            (i32::from(self.summary_scroll) + delta).clamp(0, i32::from(self.summary_max_scroll));
        self.summary_scroll = next as u16;
    }

    fn open_selected_session(&mut self) {
        if let Some(i) = self.sessions.selected_index() {
            self.open_session(i);
        }
    }

    fn open_session(&mut self, index: usize) {
        let Some(item) = self.model.sessions.get(index) else {
            return;
        };
        let Some(path) = item.path.clone() else {
            self.error = Some(format!("session {}: transcript not on disk", item.id));
            return;
        };
        let id = item.id.clone();
        let db = match gage_db::db::open_db() {
            Ok(db) => db,
            Err(e) => {
                self.error = Some(format!("open db: {e}"));
                return;
            }
        };
        let doc = match session::load_from_path(&id, &path, &db) {
            Ok(d) => d,
            Err(e) => {
                self.error = Some(format!("open session {id}: {e}"));
                return;
            }
        };
        let mut view = app::AppState::new(doc, true, app::DocSource::Path(path));
        if let Some(saved) = self.session_ui.get(&id) {
            view.restore_ui(saved);
        }
        // Keep the table selection on the opened session
        let ids: Vec<&str> = self.model.sessions.iter().map(|s| s.id.as_str()).collect();
        let current = self.sessions.selected_index().unwrap_or(0);
        self.sessions
            .select_by(index as isize - current as isize, &ids);
        self.dialog = Dialog::Session {
            index,
            view: Box::new(view),
            db,
        };
    }

    /// Route a key to the embedded session view; `Close` dismisses the
    /// dialog and ←/→ when ignored by the view steps to the
    /// neighboring session.
    fn handle_session_dialog_key(&mut self, key: KeyEvent) {
        let mut close = false;
        let mut step: isize = 0;
        let mut error: Option<String> = None;
        if let Dialog::Session { index, view, db } = &mut self.dialog {
            match app::handle_key(view, key, db) {
                Ok(app::KeyOutcome::Consumed) => {}
                Ok(app::KeyOutcome::Close) => {
                    if let Some(item) = self.model.sessions.get(*index) {
                        self.session_ui.insert(item.id.clone(), view.save_ui());
                    }
                    close = true;
                }
                Ok(app::KeyOutcome::Ignored) => {
                    step = match key.code {
                        KeyCode::Left => -1,
                        KeyCode::Right => 1,
                        _ => 0,
                    };
                    if step != 0
                        && let Some(item) = self.model.sessions.get(*index)
                    {
                        self.session_ui.insert(item.id.clone(), view.save_ui());
                    }
                }
                Err(e) => error = Some(format!("session view: {e}")),
            }
        }
        if let Some(msg) = error {
            self.error = Some(msg);
        }
        if close {
            self.dialog = Dialog::None;
        } else if step != 0
            && let Dialog::Session { index, .. } = &self.dialog
        {
            let len = self.model.sessions.len() as isize;
            let next = (*index as isize + step).clamp(0, len - 1) as usize;
            if next != *index {
                self.open_session(next);
            }
        }
    }
}

/// Returns true when the view should close.
fn handle_key(state: &mut ViewState, key: KeyEvent) -> bool {
    if let Dialog::Session { .. } = &state.dialog {
        state.handle_session_dialog_key(key);
        return false;
    }
    let ids: Vec<String> = state.model.sessions.iter().map(|s| s.id.clone()).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab | KeyCode::BackTab => state.toggle_focus(),
        KeyCode::Down | KeyCode::Char('j') => match state.focus {
            Focus::Summary => state.summary_scroll_by(1),
            Focus::Sessions => state.sessions.select_by(1, &refs),
        },
        KeyCode::Up | KeyCode::Char('k') => match state.focus {
            Focus::Summary => state.summary_scroll_by(-1),
            Focus::Sessions => state.sessions.select_by(-1, &refs),
        },
        KeyCode::PageDown => match state.focus {
            Focus::Summary => state.summary_scroll_by(i32::from(state.summary_viewport)),
            Focus::Sessions => state
                .sessions
                .select_by(state.sessions.page() as isize, &refs),
        },
        KeyCode::PageUp => match state.focus {
            Focus::Summary => state.summary_scroll_by(-i32::from(state.summary_viewport)),
            Focus::Sessions => state
                .sessions
                .select_by(-(state.sessions.page() as isize), &refs),
        },
        KeyCode::Char('g') => match state.focus {
            Focus::Summary => state.summary_scroll = 0,
            Focus::Sessions => state.sessions.select_first(&refs),
        },
        KeyCode::Char('G') => match state.focus {
            Focus::Summary => state.summary_scroll = state.summary_max_scroll,
            Focus::Sessions => state.sessions.select_last(&refs),
        },
        KeyCode::Enter if state.focus == Focus::Sessions => state.open_selected_session(),
        _ => {}
    }
    false
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [summary_area, sessions_area, footer_area] = Layout::vertical([
        Constraint::Fill(2),
        Constraint::Fill(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_summary(frame, summary_area, state);
    draw_sessions(frame, sessions_area, state);
    draw_footer(frame, footer_area, state);
    if let Dialog::Session { index, view, .. } = &mut state.dialog {
        let title = match state.model.sessions.get(*index) {
            Some(item) => format!(" Session {} ", item.id),
            None => " Session ".to_string(),
        };
        draw_session_dialog(frame, title, view);
    }
}

fn draw_summary(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Summary;
    let block = Block::bordered()
        .title(format!(
            " Scan {} ",
            gage_core::uuid::short_uuid(&state.model.scan_id)
        ))
        .border_style(styles::Panel::border(active));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = attr_lines(
        &state
            .model
            .attrs
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect::<Vec<_>>(),
    );
    if !state.model.scanner_lines.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("Scanners", styles::Text::dim())));
        for l in &state.model.scanner_lines {
            lines.push(Line::raw(l.clone()));
        }
    }
    if !state.model.cluster_lines.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Error clusters",
            styles::Text::dim(),
        )));
        for l in &state.model.cluster_lines {
            lines.push(Line::raw(l.clone()));
        }
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
    state.summary_viewport = inner.height;
    state.summary_max_scroll = total.saturating_sub(inner.height);
    if state.summary_scroll > state.summary_max_scroll {
        state.summary_scroll = state.summary_max_scroll;
    }
    frame.render_widget(paragraph.scroll((state.summary_scroll, 0)), inner);

    let mut sb = ScrollbarState::new(state.summary_max_scroll as usize)
        .position(state.summary_scroll as usize);
    frame.render_stateful_widget(
        scrollbar(active),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut sb,
    );
}

fn draw_sessions(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Sessions;
    let rows: Vec<Row> = state
        .model
        .sessions
        .iter()
        .map(|s| {
            Row::new(vec![
                Cell::from(gage_core::uuid::short_uuid(&s.id).to_string()),
                Cell::from(s.scanner.clone()),
                Cell::from(s.task.clone()),
                Cell::from(s.turns.map(|t| t.to_string()).unwrap_or_default()),
                Cell::from(s.cost.map(|c| format!("${c:.2}")).unwrap_or_default()),
                Cell::from(if s.tool_errors > 0 {
                    s.tool_errors.to_string()
                } else {
                    String::new()
                }),
                Cell::from(if s.output > 0 {
                    s.output.to_string()
                } else {
                    String::new()
                }),
            ])
        })
        .collect();
    let count = rows.len();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
        ],
    )
    .header(header_row([
        "Id", "Scanner", "Task", "Turns", "Cost", "Errs", "Out",
    ]))
    .row_highlight_style(styles::Panel::selection(active))
    .block(
        Block::bordered()
            .title(format!(" Agent sessions ({count}) "))
            .border_style(styles::Panel::border(active)),
    );
    state.sessions.render(frame, area, table, count, active);
}

/// Session dialog chrome: the inset modal with the shared session view
/// drawn inside.
fn draw_session_dialog(frame: &mut Frame, title: String, view: &mut app::AppState) {
    let area = frame.area();
    let margin_x = (area.width / 10).clamp(2, 6);
    let margin_y = (area.height / 10).clamp(1, 3);
    let rect = area.inner(Margin {
        horizontal: margin_x,
        vertical: margin_y,
    });
    frame.render_widget(Clear, rect);
    let block = Block::bordered().title(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    app::draw_in(frame, inner, view);
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &ViewState) {
    if let Some(error) = &state.error {
        let footer = Paragraph::new(Span::styled(error.clone(), styles::RunStatus::error()));
        frame.render_widget(footer, area);
        return;
    }
    let help = match (&state.dialog, state.focus) {
        (Dialog::Session { .. }, _) => {
            "q back · Tab pane · j/k g/G · Enter/Space toggle · n note · ←/→ prev/next"
        }
        (Dialog::None, Focus::Summary) => "q quit · Tab pane · j/k g/G PgUp/PgDn scroll",
        (Dialog::None, Focus::Sessions) => {
            "q quit · Tab pane · j/k g/G select · Enter open session"
        }
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

fn header_row<const N: usize>(names: [&'static str; N]) -> Row<'static> {
    Row::new(names.map(|n| Cell::from(Span::styled(n, styles::Text::dim()))))
}
