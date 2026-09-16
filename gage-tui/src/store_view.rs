//! Store viewer — a structural, payload-agnostic view of the Gage
//! store's Git object graph.
//!
//! The viewer knows the shape of an object tree (`object`, `id`,
//! `created`, `modified`, optional `prev`, optional `deleted`, and
//! `*.link` files) and displays what is there. It does not parse
//! `attrs` or interpret anything about a given object type beyond its
//! name.
//!
//! Layout: a refs table on the left, a scrollable detail pane on the
//! right, and a footer of key hints. Follows the pane/table/footer
//! shape used by `scan view` and `test view`.

use std::io;
use std::path::PathBuf;

use gage_core::datetime::ms_to_iso8601;
use gage_core::uuid::short_uuid;
use gage_store::EntryKind;
use gage_store::git::{CommitMeta, TreeEntry, list_tree_at, read_commit_at};
use gage_store::object::{
    LinkFile, ObjectHeader, ObjectRef, classify_parents_at, find_link_files_at, list_gage_refs_at,
    read_header_at, walk_prev_chain_at,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, ScrollbarState, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::hint;
use crate::item_table::{ItemTable, scrollbar};
use crate::panel::{header_row, panel_block};
use crate::session_view::{pop_keyboard_enhancements, push_keyboard_enhancements};
use crate::styles;

/// Run the store viewer against the store at `store_path`.
pub fn run(store_path: PathBuf) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = run_inner(&mut terminal, &store_path);
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result
}

fn run_inner(terminal: &mut DefaultTerminal, store_path: &std::path::Path) -> io::Result<()> {
    let mut state = ViewState::new(store_path.to_path_buf());
    state.reload();
    loop {
        terminal.draw(|frame| draw(frame, &mut state))?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            state.error = None;
            match handle_key(&mut state, key) {
                Some(ExitAction::Quit) => return Ok(()),
                None => {}
            }
        }
    }
}

enum ExitAction {
    Quit,
}

/// Which pane owns keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Refs,
    Detail,
}

struct ViewState {
    store_path: PathBuf,
    refs: Vec<ObjectRef>,
    /// Ref ids in display order, kept alongside `refs` so the id-stable
    /// table can address rows through it.
    ordered_ids: Vec<String>,
    table: ItemTable,
    focus: Focus,
    /// Scroll offset for the detail pane, in wrapped rows.
    body_scroll: u16,
    /// Max scroll for the detail pane, updated each frame from the
    /// wrapped line count and viewport height.
    body_max_scroll: u16,
    /// Cached rendered detail lines for the current selection.
    /// Rebuilt when the selection changes or the pane width changes.
    detail: Vec<Line<'static>>,
    /// Pane width the cached detail was built for. `None` on first
    /// render, then updated whenever the detail cache is rebuilt.
    detail_width: Option<u16>,
    /// A load or refresh error; shown in the footer until the next
    /// keypress.
    error: Option<String>,
}

impl ViewState {
    fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            refs: Vec::new(),
            ordered_ids: Vec::new(),
            table: ItemTable::new(),
            focus: Focus::Refs,
            body_scroll: 0,
            body_max_scroll: 0,
            detail: Vec::new(),
            detail_width: None,
            error: None,
        }
    }

    /// Reload the ref list and invalidate the detail cache.
    fn reload(&mut self) {
        match list_gage_refs_at(&self.store_path) {
            Ok(mut refs) => {
                refs.sort_by(|a, b| {
                    a.type_bucket
                        .cmp(&b.type_bucket)
                        .then_with(|| a.id.cmp(&b.id))
                });
                self.refs = refs;
                self.ordered_ids = self.refs.iter().map(|r| r.ref_name.clone()).collect();
                let ids: Vec<&str> = self.ordered_ids.iter().map(String::as_str).collect();
                self.table.update(&ids);
            }
            Err(e) => {
                self.error = Some(format!("list refs: {e}"));
                self.refs.clear();
                self.ordered_ids.clear();
                self.table.update(&[]);
            }
        }
        self.invalidate_detail();
    }

    fn invalidate_detail(&mut self) {
        self.detail_width = None;
        self.body_scroll = 0;
    }

    fn selected_ref(&self) -> Option<&ObjectRef> {
        let idx = self.table.selected_index()?;
        self.refs.get(idx)
    }

    fn cycle_focus(&mut self, delta: isize) {
        self.focus = match (self.focus, delta.signum()) {
            (Focus::Refs, 1) | (Focus::Detail, -1) => Focus::Detail,
            _ => Focus::Refs,
        };
    }
}

fn handle_key(state: &mut ViewState, key: KeyEvent) -> Option<ExitAction> {
    if let KeyCode::Char('c') = key.code
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return Some(ExitAction::Quit);
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(ExitAction::Quit),
        KeyCode::Tab => {
            state.cycle_focus(1);
            return None;
        }
        KeyCode::BackTab => {
            state.cycle_focus(-1);
            return None;
        }
        KeyCode::Char('r') => {
            state.reload();
            return None;
        }
        _ => {}
    }
    match state.focus {
        Focus::Refs => handle_refs_key(state, key),
        Focus::Detail => handle_detail_key(state, key),
    }
    None
}

fn handle_refs_key(state: &mut ViewState, key: KeyEvent) {
    let ids: Vec<&str> = state.ordered_ids.iter().map(String::as_str).collect();
    let prior = state.table.selected_index();
    let page = state.table.page() as isize;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => state.table.select_by(1, &ids),
        KeyCode::Char('k') | KeyCode::Up => state.table.select_by(-1, &ids),
        KeyCode::Char('g') | KeyCode::Home => state.table.select_first(&ids),
        KeyCode::Char('G') | KeyCode::End => state.table.select_last(&ids),
        KeyCode::PageDown => state.table.select_by(page, &ids),
        KeyCode::PageUp => state.table.select_by(-page, &ids),
        _ => {}
    }
    if state.table.selected_index() != prior {
        state.invalidate_detail();
    }
}

fn handle_detail_key(state: &mut ViewState, key: KeyEvent) {
    let page = state.body_max_scroll.max(1);
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.body_scroll = state.body_scroll.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.body_scroll = state.body_scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            state.body_scroll = state.body_scroll.saturating_add(page);
        }
        KeyCode::PageUp => {
            state.body_scroll = state.body_scroll.saturating_sub(page);
        }
        KeyCode::Char('g') | KeyCode::Home => {
            state.body_scroll = 0;
        }
        KeyCode::Char('G') | KeyCode::End => {
            state.body_scroll = state.body_max_scroll;
        }
        _ => {}
    }
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let [refs_area, detail_area] =
        Layout::horizontal([Constraint::Length(48), Constraint::Min(0)]).areas(body);
    draw_refs(frame, refs_area, state);
    draw_detail(frame, detail_area, state);
    draw_footer(frame, footer, state);
}

fn draw_refs(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Refs;
    let rows: Vec<Row> = state
        .refs
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.type_bucket.clone()),
                Cell::from(Span::styled(
                    short_uuid(&r.id).to_string(),
                    styles::Text::id(),
                )),
                Cell::from(Span::styled(
                    short_uuid(&r.tip_sha).to_string(),
                    styles::Text::dim(),
                )),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths)
        .header(header_row(["Type", "Id", "Tip"]))
        .row_highlight_style(styles::Panel::selection(active))
        .block(panel_block(
            format!(" Refs ({}) ", state.refs.len()),
            active,
        ));
    let len = state.refs.len();
    state.table.render(frame, area, table, len, active);
}

fn draw_detail(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Detail;
    let title = match state.selected_ref() {
        Some(r) => format!(" {} ", r.ref_name),
        None => " Detail ".to_string(),
    };
    let block = panel_block(title, active);
    let inner = block.inner(area);
    frame.render_widget(&block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if state.detail_width != Some(inner.width) {
        state.detail = match state.selected_ref() {
            Some(r) => render_detail(
                &state.store_path,
                r.tip_sha.clone(),
                &r.ref_name,
                inner.width,
            ),
            None => vec![Line::from(Span::styled(
                "No objects in this store.",
                styles::Text::dim(),
            ))],
        };
        state.detail_width = Some(inner.width);
    }

    let paragraph = Paragraph::new(state.detail.clone()).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
    state.body_max_scroll = total.saturating_sub(inner.height);
    if state.body_scroll > state.body_max_scroll {
        state.body_scroll = state.body_max_scroll;
    }
    frame.render_widget(paragraph.scroll((state.body_scroll, 0)), inner);

    let mut sb_state =
        ScrollbarState::new(state.body_max_scroll as usize).position(state.body_scroll as usize);
    frame.render_stateful_widget(
        scrollbar(active),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut sb_state,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &ViewState) {
    if let Some(err) = &state.error {
        let footer = Paragraph::new(Span::styled(err.clone(), styles::RunStatus::error()));
        frame.render_widget(footer, area);
        return;
    }
    let help = hint::help_line(&[
        ("Tab", "focus"),
        ("j/k g/G", ""),
        ("PgUp/PgDn", "page"),
        ("r", "refresh"),
        ("q", "quit"),
    ]);
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

/// Build the detail pane's lines for the object whose tip is `commit`.
/// A read failure at any step is surfaced as a red line rather than
/// aborting the render. `width` is the inner pane width, used so
/// full-width section headers span the pane.
fn render_detail(
    store: &std::path::Path,
    commit: String,
    ref_name: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let header = match read_header_at(store, &commit) {
        Ok(h) => h,
        Err(e) => {
            lines.push(err_line(format!("read header: {e}")));
            return lines;
        }
    };
    let commit_meta = match read_commit_at(store, &commit) {
        Ok(c) => c,
        Err(e) => {
            lines.push(err_line(format!("read commit: {e}")));
            return lines;
        }
    };

    push_header_section(&mut lines, ref_name, &commit, &header, &commit_meta, width);
    push_parents_section(&mut lines, store, &commit, width);
    push_tree_section(&mut lines, store, &commit, width);
    push_link_files_section(&mut lines, store, &commit, width);
    push_prev_chain_section(&mut lines, store, &commit, width);
    lines
}

fn push_header_section(
    lines: &mut Vec<Line<'static>>,
    ref_name: &str,
    commit: &str,
    header: &ObjectHeader,
    commit_meta: &CommitMeta,
    width: u16,
) {
    push_section_header(lines, "Object", width);
    lines.push(kv(
        "type",
        format!("{} {}", header.object_type, header.version),
    ));
    lines.push(kv("id", header.id.clone()));
    lines.push(kv("ref", ref_name.to_string()));
    lines.push(kv("commit", commit.to_string()));
    if let Some(ms) = header.created_ms {
        lines.push(kv("created", ms_to_iso8601(ms)));
    }
    if let Some(ms) = header.modified_ms {
        lines.push(kv("modified", ms_to_iso8601(ms)));
    }
    if let Some(ms) = header.deleted_ms {
        lines.push(kv("deleted", ms_to_iso8601(ms)));
    }
    if header.is_tombstone() {
        lines.push(Line::from(Span::styled(
            "  (tombstone)",
            styles::LogLevel::warn(),
        )));
    }
    lines.push(kv("author", commit_meta.author.clone()));
    lines.push(kv("committer", commit_meta.committer.clone()));
    lines.push(kv("tree", commit_meta.tree.clone()));
    let subject = commit_meta.message.lines().next().unwrap_or("").to_string();
    lines.push(kv("message", subject));
}

fn push_parents_section(
    lines: &mut Vec<Line<'static>>,
    store: &std::path::Path,
    commit: &str,
    width: u16,
) {
    push_section_header(lines, "Parents", width);
    let classified = match classify_parents_at(store, commit) {
        Ok(c) => c,
        Err(e) => {
            lines.push(err_line(format!("classify parents: {e}")));
            return;
        }
    };
    if classified.prev.is_none() && classified.links.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", styles::Text::dim())));
        return;
    }
    if let Some(prev) = &classified.prev {
        lines.push(labeled_parent("prev", prev, store));
    }
    for link in &classified.links {
        lines.push(labeled_parent(&link.link_file, &link.sha, store));
    }
    for extra in &classified.unattributed {
        lines.push(Line::from(vec![
            Span::styled("  unattributed ", styles::LogLevel::warn()),
            Span::styled(short_uuid(extra).to_string(), styles::Text::dim()),
        ]));
    }
    for missing in &classified.missing {
        lines.push(Line::from(vec![
            Span::styled("  missing parent ", styles::LogLevel::warn()),
            Span::styled(short_uuid(missing).to_string(), styles::Text::dim()),
        ]));
    }
}

fn labeled_parent(label: &str, sha: &str, store: &std::path::Path) -> Line<'static> {
    let annotation = resolve_child(store, sha);
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled(format!("{label}: "), styles::Text::dim()),
        Span::styled(short_uuid(sha).to_string(), styles::Text::id()),
    ];
    if let Some(text) = annotation {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(text, styles::Text::dim()));
    }
    Line::from(spans)
}

/// Read a commit's header and format it as a short annotation, or
/// return `None` if the SHA does not resolve to a Gage object.
fn resolve_child(store: &std::path::Path, sha: &str) -> Option<String> {
    let header = read_header_at(store, sha).ok()?;
    Some(format!(
        "{} {} {}",
        header.object_type,
        header.version,
        short_uuid(&header.id)
    ))
}

fn push_tree_section(
    lines: &mut Vec<Line<'static>>,
    store: &std::path::Path,
    commit: &str,
    width: u16,
) {
    push_section_header(lines, "Tree", width);
    let entries = match list_tree_at(store, commit) {
        Ok(t) => t,
        Err(e) => {
            lines.push(err_line(format!("list tree: {e}")));
            return;
        }
    };
    if entries.is_empty() {
        lines.push(Line::from(Span::styled("  (empty)", styles::Text::dim())));
        return;
    }
    for entry in &entries {
        lines.push(tree_line(entry));
    }
}

fn tree_line(entry: &TreeEntry) -> Line<'static> {
    let kind = entry.kind.as_str();
    let size = match entry.size {
        Some(bytes) => format!("{bytes}"),
        None => "-".to_string(),
    };
    let name = match entry.kind {
        EntryKind::Tree => format!("{}/", entry.name),
        _ => entry.name.clone(),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{kind:<4} "), styles::Text::dim()),
        Span::styled(format!("{size:>8}  "), styles::Text::dim()),
        Span::raw(name),
    ])
}

fn push_link_files_section(
    lines: &mut Vec<Line<'static>>,
    store: &std::path::Path,
    commit: &str,
    width: u16,
) {
    push_section_header(lines, "Link files", width);
    let files = match find_link_files_at(store, commit) {
        Ok(f) => f,
        Err(e) => {
            lines.push(err_line(format!("read link files: {e}")));
            return;
        }
    };
    if files.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", styles::Text::dim())));
        return;
    }
    for file in &files {
        push_link_file(lines, store, file);
    }
}

fn push_link_file(lines: &mut Vec<Line<'static>>, store: &std::path::Path, file: &LinkFile) {
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(file.path.clone(), styles::Text::accent()),
        Span::styled(format!("  ({} sha)", file.shas.len()), styles::Text::dim()),
    ]));
    if file.shas.is_empty() {
        lines.push(Line::from(Span::styled("    (empty)", styles::Text::dim())));
        return;
    }
    for sha in &file.shas {
        let annotation = resolve_child(store, sha);
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("    "),
            Span::styled(short_uuid(sha).to_string(), styles::Text::id()),
        ];
        if let Some(text) = annotation {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(text, styles::Text::dim()));
        }
        lines.push(Line::from(spans));
    }
}

fn push_prev_chain_section(
    lines: &mut Vec<Line<'static>>,
    store: &std::path::Path,
    commit: &str,
    width: u16,
) {
    push_section_header(lines, "History (prev chain)", width);
    let chain = match walk_prev_chain_at(store, commit) {
        Ok(c) => c,
        Err(e) => {
            lines.push(err_line(format!("walk prev: {e}")));
            return;
        }
    };
    if chain.len() <= 1 {
        lines.push(Line::from(Span::styled(
            "  (this is the only version)",
            styles::Text::dim(),
        )));
        return;
    }
    for (idx, sha) in chain.iter().enumerate() {
        let meta = read_commit_at(store, sha).ok();
        let subject = meta
            .as_ref()
            .map(|m| m.message.lines().next().unwrap_or("").to_string())
            .unwrap_or_default();
        let marker = if idx == 0 { "*" } else { " " };
        let time = meta
            .as_ref()
            .map(|m| ms_to_iso8601(m.committer_time_ms))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::raw(format!("  {marker} ")),
            Span::styled(short_uuid(sha).to_string(), styles::Text::id()),
            Span::raw("  "),
            Span::styled(time, styles::Text::dim()),
            Span::raw("  "),
            Span::raw(subject),
        ]));
    }
}

/// Full-width section header: a leading blank line, the label on a
/// DarkGray-bg row spanning the pane, and a trailing blank line.
/// Mirrors the layout used by `scan_view::section_header` and
/// `test_view::section_header`.
fn push_section_header(lines: &mut Vec<Line<'static>>, label: &str, width: u16) {
    lines.push(Line::raw(""));
    let pad = (width as usize).saturating_sub(1);
    lines.push(Line::from(Span::styled(
        format!(" {label:<pad$}"),
        styles::Text::header(),
    )));
    lines.push(Line::raw(""));
}

fn kv(k: &str, v: String) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{k:<10} "), styles::Text::dim()),
        Span::raw(v),
    ])
}

fn err_line(text: String) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), styles::LogLevel::error()))
}
