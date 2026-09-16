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

use gage_store::EntryKind;
use gage_store::git::{CommitMeta, TreeEntry, list_tree_at, read_commit_at};
use gage_store::object::{
    LinkFile, ObjectHeader, ObjectRef, classify_parents_at, find_link_files_at, list_gage_refs_at,
    read_header_at, walk_prev_chain_at,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::hint;
use crate::item_table::ItemTable;
use crate::session_view::{pop_keyboard_enhancements, push_keyboard_enhancements};
use crate::styles;

/// How much of a SHA to display at a glance. The full SHA is available
/// in the raw view; the short form keeps table cells and inline
/// references legible.
const SHORT_SHA: usize = 12;

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
    /// Vertical scroll offset for the detail pane.
    detail_scroll: u16,
    /// Cached rendered detail lines for the current selection. Rebuilt
    /// whenever selection or store contents change.
    detail: Vec<Line<'static>>,
    /// Last recorded viewport height of the detail pane, so paging
    /// keys know their page size.
    detail_viewport: u16,
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
            detail_scroll: 0,
            detail: Vec::new(),
            detail_viewport: 0,
            error: None,
        }
    }

    /// Reload the ref list and rebuild the detail pane.
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
        self.rebuild_detail();
    }

    fn rebuild_detail(&mut self) {
        self.detail_scroll = 0;
        self.detail = match self.selected_ref() {
            Some(r) => render_detail(&self.store_path, r.tip_sha.clone(), &r.ref_name),
            None => vec![Line::from(Span::styled(
                "No objects in this store.",
                styles::Text::dim(),
            ))],
        };
    }

    fn selected_ref(&self) -> Option<&ObjectRef> {
        let idx = self.table.selected_index()?;
        self.refs.get(idx)
    }
}

fn handle_key(state: &mut ViewState, key: KeyEvent) -> Option<ExitAction> {
    // Ctrl-C and Ctrl-D always quit, regardless of focus.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        return Some(ExitAction::Quit);
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(ExitAction::Quit),
        KeyCode::Tab => {
            state.focus = match state.focus {
                Focus::Refs => Focus::Detail,
                Focus::Detail => Focus::Refs,
            };
            None
        }
        KeyCode::Char('r') => {
            state.reload();
            None
        }
        _ => {
            match state.focus {
                Focus::Refs => handle_refs_key(state, key),
                Focus::Detail => handle_detail_key(state, key),
            }
            None
        }
    }
}

fn handle_refs_key(state: &mut ViewState, key: KeyEvent) {
    let ids: Vec<&str> = state.ordered_ids.iter().map(String::as_str).collect();
    let prior = state.table.selected_index();
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => state.table.select_by(1, &ids),
        KeyCode::Char('k') | KeyCode::Up => state.table.select_by(-1, &ids),
        KeyCode::Char('g') | KeyCode::Home => state.table.select_first(&ids),
        KeyCode::Char('G') | KeyCode::End => state.table.select_last(&ids),
        KeyCode::PageDown => {
            let page = state.table.page() as isize;
            state.table.select_by(page, &ids);
        }
        KeyCode::PageUp => {
            let page = state.table.page() as isize;
            state.table.select_by(-page, &ids);
        }
        _ => {}
    }
    if state.table.selected_index() != prior {
        state.rebuild_detail();
    }
}

fn handle_detail_key(state: &mut ViewState, key: KeyEvent) {
    let last = state
        .detail
        .len()
        .saturating_sub(state.detail_viewport.max(1) as usize) as u16;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            state.detail_scroll = state.detail_scroll.saturating_add(1).min(last);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.detail_scroll = state.detail_scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            let page = state.detail_viewport.max(1);
            state.detail_scroll = state.detail_scroll.saturating_add(page).min(last);
        }
        KeyCode::PageUp => {
            let page = state.detail_viewport.max(1);
            state.detail_scroll = state.detail_scroll.saturating_sub(page);
        }
        KeyCode::Char('g') | KeyCode::Home => {
            state.detail_scroll = 0;
        }
        KeyCode::Char('G') | KeyCode::End => {
            state.detail_scroll = last;
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
    let header = Row::new(vec![
        Cell::from("Type"),
        Cell::from("Id"),
        Cell::from("Tip"),
    ])
    .style(styles::Text::dim());
    let rows: Vec<Row> = state
        .refs
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.type_bucket.clone()),
                Cell::from(Span::styled(short_id(&r.id), styles::Text::id())),
                Cell::from(Span::styled(short_sha(&r.tip_sha), styles::Text::dim())),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" Refs ({}) ", state.refs.len()),
            styles::Panel::border(active),
        ))
        .border_style(styles::Panel::border(active));
    let widths = [
        Constraint::Length(10),
        Constraint::Length(14),
        Constraint::Length(SHORT_SHA as u16),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(styles::Panel::selection(active))
        .block(block);
    let len = state.refs.len();
    state.table.render(frame, area, table, len, active);
}

fn draw_detail(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Detail;
    let title = match state.selected_ref() {
        Some(r) => format!(" {} ", r.ref_name),
        None => " Detail ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, styles::Panel::border(active)))
        .border_style(styles::Panel::border(active));
    let inner = block.inner(area);
    state.detail_viewport = inner.height;
    let paragraph = Paragraph::new(state.detail.clone())
        .wrap(Wrap { trim: false })
        .scroll((state.detail_scroll, 0))
        .block(block);
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &ViewState) {
    let line = if let Some(err) = &state.error {
        Line::from(Span::styled(err.clone(), styles::LogLevel::error()))
    } else {
        hint::help_line(&[
            ("Tab", "focus"),
            ("j/k", "move"),
            ("PgUp/PgDn", "page"),
            ("r", "refresh"),
            ("q", "quit"),
        ])
    };
    frame.render_widget(Paragraph::new(line).style(styles::Panel::footer()), area);
}

/// Build the detail pane's lines for the object whose tip is `commit`.
/// A read failure at any step is surfaced as a red line rather than
/// aborting the render.
fn render_detail(store: &std::path::Path, commit: String, ref_name: &str) -> Vec<Line<'static>> {
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

    push_header_section(&mut lines, ref_name, &commit, &header, &commit_meta);
    lines.push(Line::from(""));
    push_parents_section(&mut lines, store, &commit);
    lines.push(Line::from(""));
    push_tree_section(&mut lines, store, &commit);
    lines.push(Line::from(""));
    push_link_files_section(&mut lines, store, &commit);
    lines.push(Line::from(""));
    push_prev_chain_section(&mut lines, store, &commit);
    lines
}

fn push_header_section(
    lines: &mut Vec<Line<'static>>,
    ref_name: &str,
    commit: &str,
    header: &ObjectHeader,
    commit_meta: &CommitMeta,
) {
    lines.push(section_header("Object"));
    lines.push(kv(
        "type",
        format!("{} {}", header.object_type, header.version),
    ));
    lines.push(kv("id", header.id.clone()));
    lines.push(kv("ref", ref_name.to_string()));
    lines.push(kv("commit", commit.to_string()));
    if let Some(ms) = header.created_ms {
        lines.push(kv("created", format_ms(ms)));
    }
    if let Some(ms) = header.modified_ms {
        lines.push(kv("modified", format_ms(ms)));
    }
    if let Some(ms) = header.deleted_ms {
        lines.push(kv("deleted", format_ms(ms)));
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

fn push_parents_section(lines: &mut Vec<Line<'static>>, store: &std::path::Path, commit: &str) {
    lines.push(section_header("Parents"));
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
            Span::styled(short_sha(extra), styles::Text::dim()),
        ]));
    }
    for missing in &classified.missing {
        lines.push(Line::from(vec![
            Span::styled("  missing parent ", styles::LogLevel::warn()),
            Span::styled(short_sha(missing), styles::Text::dim()),
        ]));
    }
}

fn labeled_parent(label: &str, sha: &str, store: &std::path::Path) -> Line<'static> {
    let annotation = resolve_child(store, sha);
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled(format!("{label}: "), styles::Text::dim()),
        Span::styled(short_sha(sha), styles::Text::id()),
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
        short_id(&header.id)
    ))
}

fn push_tree_section(lines: &mut Vec<Line<'static>>, store: &std::path::Path, commit: &str) {
    lines.push(section_header("Tree"));
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

fn push_link_files_section(lines: &mut Vec<Line<'static>>, store: &std::path::Path, commit: &str) {
    lines.push(section_header("Link files"));
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
            Span::styled(short_sha(sha), styles::Text::id()),
        ];
        if let Some(text) = annotation {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(text, styles::Text::dim()));
        }
        lines.push(Line::from(spans));
    }
}

fn push_prev_chain_section(lines: &mut Vec<Line<'static>>, store: &std::path::Path, commit: &str) {
    lines.push(section_header("History (prev chain)"));
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
            .map(|m| format_ms(m.committer_time_ms))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::raw(format!("  {marker} ")),
            Span::styled(short_sha(sha), styles::Text::id()),
            Span::raw("  "),
            Span::styled(time, styles::Text::dim()),
            Span::raw("  "),
            Span::raw(subject),
        ]));
    }
}

fn section_header(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        styles::Text::dim().add_modifier(Modifier::BOLD),
    ))
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

fn short_sha(sha: &str) -> String {
    if sha.len() > SHORT_SHA {
        sha[..SHORT_SHA].to_string()
    } else {
        sha.to_string()
    }
}

fn short_id(id: &str) -> String {
    // Object ids are 26-char Crockford base32. 12 chars keep the row
    // readable while still being unique in a typical store.
    if id.len() > SHORT_SHA {
        id[..SHORT_SHA].to_string()
    } else {
        id.to_string()
    }
}

fn format_ms(ms: i64) -> String {
    gage_core::datetime::ms_to_iso8601(ms)
}
