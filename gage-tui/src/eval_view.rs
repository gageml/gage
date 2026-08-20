//! Eval run view — a tree of the run's tests and their sessions.
//! Enter on a test opens the test dialog; Enter on a session child
//! opens the shared session viewer; Space expands/collapses. ←/→ in a
//! dialog steps depth-first through the tree's nodes, switching dialog
//! kind between test and session as needed. Follows the
//! pane/table/footer structure of `gage scan view`; the caller builds
//! an [`EvalModel`] from the eval run's structured results.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use gage_db::rusqlite::Connection;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::item_table::ItemTable;
use crate::scroll::ScrollView;
use crate::session_view::{pop_keyboard_enhancements, push_keyboard_enhancements};
use crate::{app, attrs::attr_lines, session, styles};

pub struct EvalModel {
    pub run_id: String,
    pub tests: Vec<TestItem>,
}

pub struct TestItem {
    /// Test id, `{eval}/{test}`
    pub name: String,
    /// Pass/fail from the test's score; None when the test was not
    /// scored (no `expect`, or scoring did not run)
    pub passed: Option<bool>,
    /// Score checks in evaluation order: (label, passed)
    pub checks: Vec<(String, bool)>,
    /// Assistant turns observed in the session
    pub turns: Option<u32>,
    pub exit_code: i32,
    /// Test input: the prompt, or a scanner-test config summary
    pub input: String,
    pub output: String,
    pub stderr: String,
    /// The test's sessions, in display order
    pub sessions: Vec<TestSession>,
}

pub struct TestSession {
    /// Display label, e.g. `agent 4fcb934b` or `s1 judge 5c2578a7`
    pub label: String,
    pub id: String,
    pub path: PathBuf,
}

pub fn run(model: EvalModel) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = event_loop(&mut terminal, model);
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, model: EvalModel) -> io::Result<()> {
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

/// A tree node: a test row, or one of a test's session rows.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Node {
    test: usize,
    session: Option<usize>,
}

struct ViewState {
    model: EvalModel,
    table: ItemTable,
    /// Test indexes whose session children are shown
    expanded: HashSet<usize>,
    dialog: Dialog,
    /// Scroll state and layout cache for the open test dialog
    scroll_view: ScrollView,
    /// Last operation error, shown in the footer until the next key
    error: Option<String>,
}

enum Dialog {
    None,
    /// Test detail, indexed into the model's tests
    Test {
        test: usize,
    },
    /// A test's session in the shared session view component. The
    /// connection serves the view's note operations.
    Session {
        node: Node,
        view: Box<app::AppState>,
        db: Connection,
    },
}

impl ViewState {
    fn new(model: EvalModel) -> Self {
        let mut state = Self {
            model,
            table: ItemTable::new(),
            expanded: HashSet::new(),
            dialog: Dialog::None,
            scroll_view: ScrollView::new(),
            error: None,
        };
        state.sync_table();
        state
    }

    /// Visible rows in display order, honoring expansion.
    fn visible_nodes(&self) -> Vec<Node> {
        let mut out = Vec::new();
        for (t, test) in self.model.tests.iter().enumerate() {
            out.push(Node {
                test: t,
                session: None,
            });
            if self.expanded.contains(&t) {
                for s in 0..test.sessions.len() {
                    out.push(Node {
                        test: t,
                        session: Some(s),
                    });
                }
            }
        }
        out
    }

    /// Every node in depth-first order, ignoring expansion — the ←/→
    /// stepping order.
    fn all_nodes(&self) -> Vec<Node> {
        let mut out = Vec::new();
        for (t, test) in self.model.tests.iter().enumerate() {
            out.push(Node {
                test: t,
                session: None,
            });
            for s in 0..test.sessions.len() {
                out.push(Node {
                    test: t,
                    session: Some(s),
                });
            }
        }
        out
    }

    fn node_id(&self, node: Node) -> String {
        let Some(test) = self.model.tests.get(node.test) else {
            return String::new();
        };
        match node.session.and_then(|s| test.sessions.get(s)) {
            Some(session) => format!("{}::{}", test.name, session.id),
            None => test.name.clone(),
        }
    }

    fn visible_ids(&self) -> Vec<String> {
        self.visible_nodes()
            .iter()
            .map(|n| self.node_id(*n))
            .collect()
    }

    fn selected_node(&self) -> Option<Node> {
        let i = self.table.selected_index()?;
        self.visible_nodes().get(i).copied()
    }

    fn toggle_selected(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        let Some(test) = self.model.tests.get(node.test) else {
            return;
        };
        if node.session.is_some() || test.sessions.is_empty() {
            return;
        }
        if !self.expanded.remove(&node.test) {
            self.expanded.insert(node.test);
        }
        self.sync_table();
    }

    fn sync_table(&mut self) {
        let ids = self.visible_ids();
        self.table.update(&ids_as_refs(&ids));
    }

    /// Move the table selection to `node`, expanding its parent so the
    /// row is visible.
    fn select_node(&mut self, node: Node) {
        if node.session.is_some() {
            self.expanded.insert(node.test);
        }
        self.sync_table();
        let ids = self.visible_ids();
        let target_id = self.node_id(node);
        let Some(target) = ids.iter().position(|id| *id == target_id) else {
            return;
        };
        let current = self.table.selected_index().unwrap_or(0);
        self.table
            .select_by(target as isize - current as isize, &ids_as_refs(&ids));
    }

    fn open_selected(&mut self) {
        if let Some(node) = self.selected_node() {
            self.open_node(node);
        }
    }

    fn open_node(&mut self, node: Node) {
        self.select_node(node);
        match node.session {
            None => {
                self.scroll_view.reset();
                self.dialog = Dialog::Test { test: node.test };
            }
            Some(s) => {
                let Some((id, path)) = self
                    .model
                    .tests
                    .get(node.test)
                    .and_then(|t| t.sessions.get(s))
                    .map(|x| (x.id.clone(), x.path.clone()))
                else {
                    return;
                };
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
                let view = app::AppState::new(doc, true, app::DocSource::Path(path));
                self.dialog = Dialog::Session {
                    node,
                    view: Box::new(view),
                    db,
                };
            }
        }
    }

    /// Step the open dialog to the neighboring node in depth-first
    /// order, switching between test and session dialogs as needed.
    fn step_dialog(&mut self, delta: isize) {
        let current = match &self.dialog {
            Dialog::Test { test } => Node {
                test: *test,
                session: None,
            },
            Dialog::Session { node, .. } => *node,
            Dialog::None => return,
        };
        let nodes = self.all_nodes();
        let Some(pos) = nodes.iter().position(|n| *n == current) else {
            return;
        };
        let next = (pos as isize + delta).clamp(0, nodes.len() as isize - 1) as usize;
        if next != pos
            && let Some(node) = nodes.get(next)
        {
            self.open_node(*node);
        }
    }

    /// Route a key to the embedded session view; `Close` dismisses the
    /// dialog and ←/→ when ignored by the view steps the tree.
    fn handle_session_dialog_key(&mut self, key: KeyEvent) {
        let mut close = false;
        let mut step: isize = 0;
        let mut error: Option<String> = None;
        if let Dialog::Session { view, db, .. } = &mut self.dialog {
            match app::handle_key(view, key, db) {
                Ok(app::KeyOutcome::Consumed) => {}
                Ok(app::KeyOutcome::Close) => close = true,
                Ok(app::KeyOutcome::Ignored) => {
                    step = match key.code {
                        KeyCode::Left => -1,
                        KeyCode::Right => 1,
                        _ => 0,
                    };
                }
                Err(e) => error = Some(format!("session view: {e}")),
            }
        }
        if let Some(msg) = error {
            self.error = Some(msg);
        }
        if close {
            self.dialog = Dialog::None;
        } else if step != 0 {
            self.step_dialog(step);
        }
    }
}

fn ids_as_refs(ids: &[String]) -> Vec<&str> {
    ids.iter().map(String::as_str).collect()
}

/// Returns true when the view should close.
fn handle_key(state: &mut ViewState, key: KeyEvent) -> bool {
    match &state.dialog {
        Dialog::Session { .. } => {
            state.handle_session_dialog_key(key);
            return false;
        }
        Dialog::Test { .. } => {
            let page = state.scroll_view.page() as isize;
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => state.dialog = Dialog::None,
                KeyCode::Down | KeyCode::Char('j') => state.scroll_view.scroll_by(1),
                KeyCode::Up | KeyCode::Char('k') => state.scroll_view.scroll_by(-1),
                KeyCode::PageDown => state.scroll_view.scroll_by(page),
                KeyCode::PageUp => state.scroll_view.scroll_by(-page),
                KeyCode::Char('g') => state.scroll_view.scroll_to_top(),
                KeyCode::Char('G') => state.scroll_view.scroll_to_bottom(),
                KeyCode::Right => state.step_dialog(1),
                KeyCode::Left => state.step_dialog(-1),
                _ => {}
            }
            return false;
        }
        Dialog::None => {}
    }
    let ids = state.visible_ids();
    let refs = ids_as_refs(&ids);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Down | KeyCode::Char('j') => state.table.select_by(1, &refs),
        KeyCode::Up | KeyCode::Char('k') => state.table.select_by(-1, &refs),
        KeyCode::PageDown => state.table.select_by(state.table.page() as isize, &refs),
        KeyCode::PageUp => state.table.select_by(-(state.table.page() as isize), &refs),
        KeyCode::Char('g') => state.table.select_first(&refs),
        KeyCode::Char('G') => state.table.select_last(&refs),
        KeyCode::Char(' ') => state.toggle_selected(),
        KeyCode::Enter => state.open_selected(),
        _ => {}
    }
    false
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [tests_area, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    draw_tree(frame, tests_area, state);
    draw_footer(frame, footer_area, state);
    let ViewState {
        dialog,
        scroll_view,
        model,
        ..
    } = state;
    match dialog {
        Dialog::Test { test } => {
            if let Some(test) = model.tests.get(*test) {
                scroll_view.render_modal(frame, format!(" Test {} ", test.name), |width| {
                    test_sections(test, width as usize)
                });
            }
        }
        Dialog::Session { node, view, .. } => {
            let label = node.session.and_then(|s| {
                model
                    .tests
                    .get(node.test)
                    .and_then(|t| t.sessions.get(s).map(|x| x.label.clone()))
            });
            let title = match label {
                Some(label) => format!(" {label} "),
                None => " Session ".to_string(),
            };
            draw_session_dialog(frame, title, view);
        }
        Dialog::None => {}
    }
}

fn draw_tree(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let nodes = state.visible_nodes();
    let rows: Vec<Row> = nodes
        .iter()
        .filter_map(|node| {
            let test = state.model.tests.get(node.test)?;
            let row = match node.session {
                None => {
                    let result = match test.passed {
                        Some(true) => "✓",
                        Some(false) => "✗",
                        None => "-",
                    };
                    let glyph = if test.sessions.is_empty() {
                        "  "
                    } else if state.expanded.contains(&node.test) {
                        "▼ "
                    } else {
                        "▶ "
                    };
                    Row::new(vec![
                        Cell::from(result.to_string()),
                        Cell::from(format!("{glyph}{}", test.name)),
                    ])
                }
                Some(s) => Row::new(vec![
                    Cell::from(String::new()),
                    Cell::from(Span::styled(
                        format!("    {}", test.sessions.get(s)?.label),
                        styles::Text::dim(),
                    )),
                ]),
            };
            Some(row)
        })
        .collect();
    let count = rows.len();
    let table = Table::new(rows, [Constraint::Length(1), Constraint::Fill(1)])
        .header(header_row(["", "Test"]))
        .row_highlight_style(styles::Panel::selection(true))
        .block(panel_block(
            format!(
                " Eval {} · Tests ({}) ",
                gage_core::uuid::short_uuid(&state.model.run_id),
                state.model.tests.len()
            ),
            true,
        ));
    state.table.render(frame, area, table, count, true);
}

/// Test dialog sections: result attributes, then the score checks,
/// input, output, and stderr under full-width banners.
fn test_sections(test: &TestItem, width: usize) -> Vec<Vec<Line<'static>>> {
    let mut out = Vec::new();

    let result = match test.passed {
        Some(true) => "✓ pass",
        Some(false) => "✗ fail",
        None => "not scored",
    };
    let turns = test.turns.map(|t| t.to_string()).unwrap_or_default();
    let exit = test.exit_code.to_string();
    let mut attrs: Vec<(&'static str, &str)> = vec![("Result", result)];
    if !turns.is_empty() {
        attrs.push(("Turns", &turns));
    }
    attrs.push(("Exit", &exit));
    out.push(attr_lines(&attrs));

    if !test.checks.is_empty() {
        let mut content = Vec::new();
        for (label, passed) in &test.checks {
            let (glyph, style) = if *passed {
                ("✓ ", styles::Text::dim())
            } else {
                ("✗ ", styles::RunStatus::error())
            };
            content.push(Line::from(vec![
                Span::styled(glyph, style),
                Span::raw(label.clone()),
            ]));
        }
        out.push(bar_section("Checks", content, width));
    }

    out.push(bar_section("Input", text_lines(&test.input), width));
    out.push(bar_section("Output", text_lines(&test.output), width));
    if !test.stderr.trim().is_empty() {
        out.push(bar_section("Error", text_lines(&test.stderr), width));
    }
    out
}

/// Full-width banner over its content, one blank line below the
/// banner — the layout used by the `gage scan view` issue dialog.
fn bar_section(
    caption: &'static str,
    content: Vec<Line<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            format!(" {caption:<pad$}", pad = width.saturating_sub(1)),
            styles::Text::header(),
        )),
        Line::raw(""),
    ];
    lines.extend(content);
    lines
}

fn text_lines(text: &str) -> Vec<Line<'static>> {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return vec![Line::from(Span::styled("(empty)", styles::Text::dim()))];
    }
    trimmed.lines().map(|l| Line::raw(l.to_string())).collect()
}

/// Session dialog chrome: the same inset modal the content dialogs
/// use, with the shared session view drawn inside.
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
    let help = match state.dialog {
        Dialog::Test { .. } => "q close · ↑/↓ scroll · ←/→ prev/next",
        Dialog::Session { .. } => {
            "q back · Tab pane · j/k g/G · Enter/Space toggle · n note · ←/→ prev/next"
        }
        Dialog::None => "q quit · ↑/↓ select · Space expand · Enter open",
    };
    let footer = Paragraph::new(Span::styled(help, styles::Panel::footer()));
    frame.render_widget(footer, area);
}

fn panel_block(title: String, active: bool) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(styles::Panel::border(active))
}

fn header_row<const N: usize>(names: [&'static str; N]) -> Row<'static> {
    Row::new(names.map(|n| Cell::from(Span::styled(n, styles::Text::dim()))))
}
