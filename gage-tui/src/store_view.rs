//! Store viewer — a structural, payload-agnostic view of the Gage
//! store's Git object graph.
//!
//! The left pane is a lazy tree: one root per object ref, expanding
//! into the object's commit tree, with subtrees expanding on demand.
//! Nothing below a node is read until the node is expanded or
//! selected. The right pane shows what the selected row is: the
//! object's structural detail (header, parents, link files, history),
//! a tree's direct entries as a table, or a blob's text.
//!
//! The viewer knows the shape of an object tree (`type`, `id`,
//! `created`, `modified`, optional `parent`, optional `deleted`, and
//! `*.link` files) and displays what is there. It does not parse
//! `attrs.json` or interpret anything about a given object type beyond
//! its name.

use std::collections::HashMap;
use std::io;

use gage_core::datetime::ms_to_iso8601;
use gage_core::uuid::short_uuid;
use gage_store::object::{LinkFile, ObjectHeader, ObjectRef};
use gage_store::{CommitMeta, EntryKind, Store, StoreError, TreeEntry};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::hint;
use crate::item_table::ItemTable;
use crate::panel::{header_row, panel_block};
use crate::scroll::ScrollView;
use crate::session_view::{pop_keyboard_enhancements, push_keyboard_enhancements};
use crate::styles;
use crate::syntax::Highlighter;
use crate::text::hard_wrap;
use crate::tree::{Collapse, Expand, Tree};

/// Run the store viewer against an opened store.
pub fn run(store: Store) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let enhanced_keys = push_keyboard_enhancements();
    let result = run_inner(&mut terminal, store);
    if enhanced_keys {
        pop_keyboard_enhancements();
    }
    ratatui::restore();
    result
}

fn run_inner(terminal: &mut DefaultTerminal, store: Store) -> io::Result<()> {
    let mut state = ViewState::new(store);
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Tree,
    Detail,
}

/// A node of the left pane
enum Node {
    /// A root grouping objects by whether their tip is a tombstone
    Group {
        label: &'static str,
        /// Objects under the group
        count: usize,
    },
    /// One object ref under a group, with the type name read from its
    /// tip
    Object {
        object_ref: ObjectRef,
        /// Type name without the `gage::` prefix, or `?` when the tip
        /// is not a readable Gage object.
        type_name: String,
    },
    /// An entry of a commit or tree below a root
    Entry(TreeEntry),
}

/// Bytes of the selected blob, classified once
enum BlobContent {
    Text(String),
    Binary,
}

/// Bytes scanned for a NUL when deciding whether a blob is text
const BINARY_PROBE_BYTES: usize = 8 * 1024;
/// Rows per scroll section for blob text, well under the per-section
/// row limit of the scroll view
const BLOB_SECTION_ROWS: usize = 1000;

struct ViewState {
    store: Store,
    tree: Tree<Node>,
    table: ItemTable,
    focus: Focus,
    /// Direct entries of each tree listed so far, by tree or commit
    /// SHA, name-sorted
    listings: HashMap<String, Vec<TreeEntry>>,
    /// Right pane scroll state for object detail and blob text
    scroll: ScrollView,
    /// Right pane selection for a tree listing, keyed by entry name
    listing_table: ItemTable,
    /// The selected blob's content, by SHA, read on selection
    blob: Option<(String, BlobContent)>,
    highlighter: Highlighter,
    /// A load or refresh error; shown in the footer until the next
    /// keypress.
    error: Option<String>,
    /// Object refs at the last reload, live and deleted
    object_count: usize,
}

impl ViewState {
    fn new(store: Store) -> Self {
        // Blob text can be megabytes of long lines; every builder
        // pre-wraps to the width so the scroll view walks no text
        let mut scroll = ScrollView::new();
        scroll.set_wrap(false);
        Self {
            store,
            tree: Tree::new(),
            table: ItemTable::new(),
            focus: Focus::Tree,
            listings: HashMap::new(),
            scroll,
            listing_table: ItemTable::new(),
            blob: None,
            highlighter: Highlighter::new(),
            error: None,
            object_count: 0,
        }
    }

    /// Rebuild the tree from the ref list: a `Live` group and a
    /// `Deleted` group, each expanded over its objects. Expansion
    /// state is discarded; listings already read stay cached by SHA.
    fn reload(&mut self) {
        self.tree.clear();
        self.object_count = 0;
        match self.store.list_object_refs() {
            Ok(refs) => {
                let mut objects: Vec<(ObjectRef, String, bool)> = refs
                    .into_iter()
                    .map(|object_ref| {
                        let (type_name, deleted) = match self.store.read_header(&object_ref.tip_sha)
                        {
                            Ok(h) => (type_display(&h.object_type), h.is_tombstone()),
                            Err(_) => ("?".to_string(), false),
                        };
                        (object_ref, type_name, deleted)
                    })
                    .collect();
                objects.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.id.cmp(&b.0.id)));
                self.object_count = objects.len();
                let (deleted, live): (Vec<_>, Vec<_>) =
                    objects.into_iter().partition(|(_, _, deleted)| *deleted);
                self.add_group("Live", live);
                self.add_group("Deleted", deleted);
            }
            Err(e) => {
                self.error = Some(format!("list refs: {e}"));
            }
        }
        self.sync_table();
        self.selection_changed();
    }

    /// Add a group root expanded over `objects`. An empty group is a
    /// leaf.
    fn add_group(&mut self, label: &'static str, objects: Vec<(ObjectRef, String, bool)>) {
        let count = objects.len();
        let node = self.tree.add_root(
            label.to_ascii_lowercase(),
            Node::Group { label, count },
            count > 0,
        );
        if count == 0 {
            return;
        }
        let children = objects
            .into_iter()
            .map(|(object_ref, type_name, _)| {
                (
                    object_ref.ref_name.clone(),
                    Node::Object {
                        object_ref,
                        type_name,
                    },
                    true,
                )
            })
            .collect();
        self.tree.set_children(node, children);
    }

    /// Reconcile the selection table with the visible rows
    fn sync_table(&mut self) {
        let keys = self.tree.visible_keys();
        self.table.update(&keys);
    }

    fn selection_changed(&mut self) {
        self.scroll.reset();
        self.listing_table = ItemTable::new();
        self.blob = None;
    }

    /// The SHA of the selected tree row, when a tree is selected
    fn selected_tree_sha(&self) -> Option<String> {
        match self.selected_node()? {
            Node::Entry(entry) if entry.kind == EntryKind::Tree => Some(entry.sha.clone()),
            _ => None,
        }
    }

    fn selected_node(&self) -> Option<&Node> {
        let idx = self.table.selected_index()?;
        let row = self.tree.row(idx)?;
        self.tree.data(row.node)
    }

    fn selected_key(&self) -> Option<&str> {
        let idx = self.table.selected_index()?;
        let row = self.tree.row(idx)?;
        self.tree.key(row.node)
    }

    fn toggle_selected(&mut self) {
        let Some(idx) = self.table.selected_index() else {
            return;
        };
        let outcome = self.tree.toggle(idx);
        self.after_expand(outcome);
    }

    fn expand_selected(&mut self) {
        let Some(idx) = self.table.selected_index() else {
            return;
        };
        let outcome = self.tree.expand(idx);
        self.after_expand(outcome);
    }

    fn after_expand(&mut self, outcome: Expand) {
        if let Expand::NeedsChildren(node) = outcome {
            self.load_children(node);
        }
        self.sync_table();
    }

    /// Left: collapse the selected row, or move to its parent when
    /// it is a leaf or already collapsed.
    fn collapse_selected(&mut self) {
        let Some(idx) = self.table.selected_index() else {
            return;
        };
        match self.tree.collapse(idx) {
            Collapse::Collapsed => self.sync_table(),
            Collapse::SelectParent(parent) => {
                let keys = self.tree.visible_keys();
                let delta = parent as isize - idx as isize;
                self.table.select_by(delta, &keys);
                self.selection_changed();
            }
            Collapse::Nothing => {}
        }
    }

    /// Read the tree behind `node` and hand its entries to the tree
    /// as children.
    fn load_children(&mut self, node: usize) {
        let (sha, key) = match (self.tree.data(node), self.tree.key(node)) {
            (Some(Node::Object { object_ref, .. }), Some(key)) => {
                (object_ref.tip_sha.clone(), key.to_string())
            }
            (Some(Node::Entry(entry)), Some(key)) => (entry.sha.clone(), key.to_string()),
            // A group's children are supplied at reload
            _ => return,
        };
        let entries = match self.listing(&sha) {
            Ok(entries) => entries.clone(),
            Err(e) => {
                self.error = Some(format!("list tree: {e}"));
                return;
            }
        };
        let children = entries
            .into_iter()
            .map(|entry| {
                let child_key = format!("{key}/{}", entry.name);
                let expandable = entry.kind == EntryKind::Tree;
                (child_key, Node::Entry(entry), expandable)
            })
            .collect();
        self.tree.set_children(node, children);
    }

    /// The direct entries under `sha`, read once and cached, trees
    /// before blobs and each group name-sorted. Sizes come from git's
    /// object info, not from reading content.
    fn listing(&mut self, sha: &str) -> Result<&Vec<TreeEntry>, StoreError> {
        if !self.listings.contains_key(sha) {
            let mut entries = self.store.list_tree(sha)?;
            // Trees first, then blobs, each name-sorted
            entries.sort_by(|a, b| {
                let is_blob = |e: &TreeEntry| e.kind != EntryKind::Tree;
                is_blob(a)
                    .cmp(&is_blob(b))
                    .then_with(|| a.name.cmp(&b.name))
            });
            self.listings.insert(sha.to_string(), entries);
        }
        Ok(self
            .listings
            .get(sha)
            .expect("listing inserted above when absent"))
    }

    /// The selected blob's content, read on first request
    fn blob_content(&mut self, sha: &str) -> Result<&BlobContent, StoreError> {
        if !matches!(&self.blob, Some((s, _)) if s == sha) {
            let bytes = self.store.read_blob_bytes(sha)?;
            self.blob = Some((sha.to_string(), classify_blob(bytes)));
        }
        Ok(&self
            .blob
            .as_ref()
            .expect("blob content set above when absent")
            .1)
    }

    /// Two panes: either direction moves to the other pane
    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Tree => Focus::Detail,
            Focus::Detail => Focus::Tree,
        };
    }
}

/// Text when the bytes are valid UTF-8 with no NUL in the probe
/// window, else binary.
fn classify_blob(bytes: Vec<u8>) -> BlobContent {
    if bytes.iter().take(BINARY_PROBE_BYTES).any(|&b| b == 0) {
        return BlobContent::Binary;
    }
    match String::from_utf8(bytes) {
        Ok(text) => BlobContent::Text(text),
        Err(_) => BlobContent::Binary,
    }
}

fn handle_key(state: &mut ViewState, key: KeyEvent) -> Option<ExitAction> {
    if let KeyCode::Char('c') = key.code
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return Some(ExitAction::Quit);
    }
    // Shift+Tab arrives as `BackTab`, or under the enhanced keyboard
    // protocol as `Tab` with the shift modifier
    let shift_tab = key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
    if shift_tab {
        state.cycle_focus();
        return None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(ExitAction::Quit),
        KeyCode::Tab => {
            state.cycle_focus();
            return None;
        }
        KeyCode::Char('r') => {
            state.reload();
            return None;
        }
        _ => {}
    }
    match state.focus {
        Focus::Tree => handle_tree_key(state, key),
        Focus::Detail => handle_detail_key(state, key),
    }
    None
}

fn handle_tree_key(state: &mut ViewState, key: KeyEvent) {
    let prior = state.table.selected_index();
    let page = state.table.page() as isize;
    let keys = state.tree.visible_keys();
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => state.table.select_by(1, &keys),
        KeyCode::Char('k') | KeyCode::Up => state.table.select_by(-1, &keys),
        KeyCode::Char('g') | KeyCode::Home => state.table.select_first(&keys),
        KeyCode::Char('G') | KeyCode::End => state.table.select_last(&keys),
        KeyCode::PageDown => state.table.select_by(page, &keys),
        KeyCode::PageUp => state.table.select_by(-page, &keys),
        KeyCode::Char(' ') | KeyCode::Enter => {
            drop(keys);
            state.toggle_selected();
            return;
        }
        KeyCode::Right => {
            drop(keys);
            state.expand_selected();
            return;
        }
        KeyCode::Left => {
            drop(keys);
            state.collapse_selected();
            return;
        }
        _ => {}
    }
    if state.table.selected_index() != prior {
        state.selection_changed();
    }
}

fn handle_detail_key(state: &mut ViewState, key: KeyEvent) {
    if let Some(sha) = state.selected_tree_sha() {
        handle_listing_key(state, &sha, key);
        return;
    }
    let page = state.scroll.page() as isize;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => state.scroll.scroll_by(1),
        KeyCode::Char('k') | KeyCode::Up => state.scroll.scroll_by(-1),
        KeyCode::PageDown => state.scroll.scroll_by(page),
        KeyCode::PageUp => state.scroll.scroll_by(-page),
        KeyCode::Char('g') | KeyCode::Home => state.scroll.scroll_to_top(),
        KeyCode::Char('G') | KeyCode::End => state.scroll.scroll_to_bottom(),
        _ => {}
    }
}

/// Navigation in a tree listing: the same keys as the tree pane,
/// moving the listing's own selection.
fn handle_listing_key(state: &mut ViewState, sha: &str, key: KeyEvent) {
    let names: Vec<String> = match state.listing(sha) {
        Ok(entries) => entries.iter().map(|e| e.name.clone()).collect(),
        Err(_) => return,
    };
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let page = state.listing_table.page() as isize;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => state.listing_table.select_by(1, &names),
        KeyCode::Char('k') | KeyCode::Up => state.listing_table.select_by(-1, &names),
        KeyCode::Char('g') | KeyCode::Home => state.listing_table.select_first(&names),
        KeyCode::Char('G') | KeyCode::End => state.listing_table.select_last(&names),
        KeyCode::PageDown => state.listing_table.select_by(page, &names),
        KeyCode::PageUp => state.listing_table.select_by(-page, &names),
        _ => {}
    }
}

fn draw(frame: &mut Frame, state: &mut ViewState) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
    let [tree_area, detail_area] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).areas(body);
    draw_tree(frame, tree_area, state);
    draw_detail(frame, detail_area, state);
    draw_footer(frame, footer, state);
}

fn draw_tree(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Tree;
    let rows: Vec<Row> = state
        .tree
        .rows()
        .iter()
        .filter_map(|row| {
            let node = state.tree.data(row.node)?;
            let indent = "  ".repeat(row.level);
            let glyph = if !row.expandable {
                "  "
            } else if row.expanded {
                "▼ "
            } else {
                "▶ "
            };
            let mut spans = vec![Span::raw(format!("{indent}{glyph}"))];
            match node {
                Node::Group { label, .. } => spans.push(Span::raw(*label)),
                Node::Object {
                    object_ref,
                    type_name,
                } => {
                    spans.push(Span::raw(format!("{type_name} ")));
                    spans.push(Span::styled(
                        short_uuid(&object_ref.id).to_string(),
                        styles::Text::id(),
                    ));
                }
                Node::Entry(entry) => match entry.kind {
                    EntryKind::Tree => spans.push(Span::styled(
                        format!("{}/", entry.name),
                        styles::Text::accent(),
                    )),
                    _ => spans.push(Span::raw(entry.name.clone())),
                },
            }
            Some(Row::new(vec![Cell::from(Line::from(spans))]))
        })
        .collect();
    let count = rows.len();
    let table = Table::new(rows, [Constraint::Fill(1)])
        .row_highlight_style(styles::Panel::selection(active))
        .block(panel_block(
            format!(" Objects ({}) ", state.object_count),
            active,
        ));
    state.table.render(frame, area, table, count, active);
}

fn draw_detail(frame: &mut Frame, area: Rect, state: &mut ViewState) {
    let active = state.focus == Focus::Detail;
    let title = match state.selected_key() {
        Some(key) => format!(" {key} "),
        None => " Detail ".to_string(),
    };
    let block = panel_block(title, active);
    let inner = block.inner(area);
    if inner.width == 0 || inner.height == 0 {
        frame.render_widget(&block, area);
        return;
    }
    // A tree listing renders the block as its table's own so the
    // selection table can place the scrollbar; every other mode
    // draws it here
    if state.selected_tree_sha().is_none() {
        frame.render_widget(&block, area);
    }

    enum Selected {
        None,
        Group { label: &'static str, count: usize },
        Object { commit: String, ref_name: String },
        Tree(String),
        Blob { sha: String, name: String },
        Commit(String),
    }
    let selected = match state.selected_node() {
        None => Selected::None,
        Some(Node::Group { label, count }) => Selected::Group {
            label,
            count: *count,
        },
        Some(Node::Object { object_ref, .. }) => Selected::Object {
            commit: object_ref.tip_sha.clone(),
            ref_name: object_ref.ref_name.clone(),
        },
        Some(Node::Entry(entry)) => match entry.kind {
            EntryKind::Tree => Selected::Tree(entry.sha.clone()),
            EntryKind::Blob => Selected::Blob {
                sha: entry.sha.clone(),
                name: entry.name.clone(),
            },
            EntryKind::Commit => Selected::Commit(entry.sha.clone()),
        },
    };

    match selected {
        Selected::None => {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "No objects in this store.",
                    styles::Text::dim(),
                )),
                inner,
            );
        }
        Selected::Group { label, count } => {
            let noun = if count == 1 { "object" } else { "objects" };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("{count} {} {noun}", label.to_ascii_lowercase()),
                    styles::Text::dim(),
                )),
                inner,
            );
        }
        Selected::Object { commit, ref_name } => {
            let store = &state.store;
            state.scroll.render_in(frame, area, inner, active, |width| {
                detail_sections(store, &commit, &ref_name, width)
                    .into_iter()
                    .map(|lines| hard_wrap(lines, width as usize))
                    .collect()
            });
        }
        Selected::Tree(sha) => match state.listing(&sha).map(|_| ()) {
            Ok(()) => {
                let ViewState {
                    listings,
                    listing_table,
                    ..
                } = state;
                let entries = listings
                    .get(&sha)
                    .expect("listing cached by the call above");
                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                let len = names.len();
                let table = listing_table_widget(entries)
                    .row_highlight_style(styles::Panel::selection(active))
                    .block(block);
                listing_table.update(&names);
                listing_table.render(frame, area, table, len, active);
            }
            Err(e) => {
                frame.render_widget(Paragraph::new(err_line(format!("list tree: {e}"))), inner);
            }
        },
        Selected::Blob { sha, name } => {
            if let Err(e) = state.blob_content(&sha) {
                frame.render_widget(Paragraph::new(err_line(format!("read blob: {e}"))), inner);
                return;
            }
            let ViewState {
                scroll,
                blob,
                highlighter,
                ..
            } = state;
            match blob {
                Some((_, BlobContent::Text(text))) => {
                    scroll.render_in(frame, area, inner, active, |width| {
                        blob_sections(text, &name, highlighter, width as usize)
                    });
                }
                Some((_, BlobContent::Binary)) => {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            "This appears to be a binary file",
                            styles::Text::dim(),
                        )),
                        inner,
                    );
                }
                None => {}
            }
        }
        Selected::Commit(sha) => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("commit entry ", styles::Text::dim()),
                    Span::styled(short_uuid(&sha).to_string(), styles::Text::id()),
                ])),
                inner,
            );
        }
    }
}

/// The direct entries of a tree as a table: what git lists without
/// reading any content.
fn listing_table_widget(entries: &[TreeEntry]) -> Table<'static> {
    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| {
            let name = match entry.kind {
                EntryKind::Tree => Cell::from(Span::styled(
                    format!("{}/", entry.name),
                    styles::Text::accent(),
                )),
                _ => Cell::from(entry.name.clone()),
            };
            let size = entry
                .size
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            Row::new(vec![
                name,
                Cell::from(Span::styled(
                    entry.kind.as_str().to_string(),
                    styles::Text::dim(),
                )),
                Cell::from(Span::styled(entry.mode.clone(), styles::Text::dim())),
                Cell::from(Line::from(size).right_aligned()),
                Cell::from(Span::styled(
                    short_uuid(&entry.sha).to_string(),
                    styles::Text::dim(),
                )),
            ])
        })
        .collect();
    Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(header_row(["Name", "Kind", "Mode", "Size", "Sha"]))
}

/// Blob text pre-wrapped to `width`, as sections of at most
/// `BLOB_SECTION_ROWS` rows. A `.json` blob that parses is
/// pretty-printed and highlighted; one that does not parse is shown as
/// it is.
fn blob_sections(
    text: &str,
    name: &str,
    highlighter: &Highlighter,
    width: usize,
) -> Vec<Vec<Line<'static>>> {
    if text.is_empty() {
        return vec![vec![Line::from(Span::styled(
            "(empty)",
            styles::Text::dim(),
        ))]];
    }
    let lines: Vec<Line<'static>> = match pretty_json(text, name) {
        Some(pretty) => highlighter.highlight(&pretty, "json"),
        None => text.lines().map(|l| Line::raw(l.to_string())).collect(),
    };
    hard_wrap(lines, width)
        .chunks(BLOB_SECTION_ROWS)
        .map(|chunk| chunk.to_vec())
        .collect()
}

/// The pretty-printed form of a `.json` blob, or `None` when the name
/// has another extension or the text is not JSON
fn pretty_json(text: &str, name: &str) -> Option<String> {
    if !name.ends_with(".json") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    serde_json::to_string_pretty(&value).ok()
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
        ("Space", "toggle"),
        ("→/←", "expand/collapse"),
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

/// The detail pane's sections for the object whose tip is `commit`.
/// A read failure at any step is surfaced as a red line rather than
/// aborting the render. `width` is the inner pane width, used so
/// full-width section headers span the pane.
fn detail_sections(
    store: &Store,
    commit: &str,
    ref_name: &str,
    width: u16,
) -> Vec<Vec<Line<'static>>> {
    let header = match store.read_header(commit) {
        Ok(h) => h,
        Err(e) => return vec![vec![err_line(format!("read header: {e}"))]],
    };
    let commit_meta = match store.read_commit(commit) {
        Ok(c) => c,
        Err(e) => return vec![vec![err_line(format!("read commit: {e}"))]],
    };
    vec![
        header_section(ref_name, commit, &header, &commit_meta, width),
        parents_section(store, commit, width),
        link_files_section(store, commit, width),
        parent_chain_section(store, commit, width),
    ]
}

fn header_section(
    ref_name: &str,
    commit: &str,
    header: &ObjectHeader,
    commit_meta: &CommitMeta,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = section_header("Object", width);
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
        let mut line = kv("deleted", ms_to_iso8601(ms));
        line.push_span(Span::styled(" (tombstone)", styles::LogLevel::warn()));
        lines.push(line);
    }
    lines.push(kv("author", commit_meta.author.clone()));
    lines.push(kv("committer", commit_meta.committer.clone()));
    lines.push(kv("tree", commit_meta.tree.clone()));
    let subject = commit_meta.message.lines().next().unwrap_or("").to_string();
    lines.push(kv("message", subject));
    lines
}

fn parents_section(store: &Store, commit: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = section_header("Parents", width);
    let classified = match store.classify_parents(commit) {
        Ok(c) => c,
        Err(e) => {
            lines.push(err_line(format!("classify parents: {e}")));
            return lines;
        }
    };
    if classified.parent.is_none() && classified.links.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", styles::Text::dim())));
        return lines;
    }
    if let Some(parent) = &classified.parent {
        lines.push(labeled_parent("parent", parent, store));
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
    lines
}

fn labeled_parent(label: &str, sha: &str, store: &Store) -> Line<'static> {
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
fn resolve_child(store: &Store, sha: &str) -> Option<String> {
    let header = store.read_header(sha).ok()?;
    Some(format!(
        "{} {} {}",
        header.object_type,
        header.version,
        short_uuid(&header.id)
    ))
}

fn link_files_section(store: &Store, commit: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = section_header("Link files", width);
    let files = match store.find_link_files(commit) {
        Ok(f) => f,
        Err(e) => {
            lines.push(err_line(format!("read link files: {e}")));
            return lines;
        }
    };
    if files.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", styles::Text::dim())));
        return lines;
    }
    for file in &files {
        push_link_file(&mut lines, store, file);
    }
    lines
}

fn push_link_file(lines: &mut Vec<Line<'static>>, store: &Store, file: &LinkFile) {
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

fn parent_chain_section(store: &Store, commit: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = section_header("History", width);
    let chain = match store.walk_parent_chain(commit) {
        Ok(c) => c,
        Err(e) => {
            lines.push(err_line(format!("walk parents: {e}")));
            return lines;
        }
    };
    if chain.len() <= 1 {
        lines.push(Line::from(Span::styled(
            "  (this is the only version)",
            styles::Text::dim(),
        )));
        return lines;
    }
    for (idx, sha) in chain.iter().enumerate() {
        let meta = store.read_commit(sha).ok();
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
    lines
}

/// Full-width section header: a blank line, the label on a
/// DarkGray-bg row spanning the pane, and a blank line. The leading
/// blank is the padding below the previous section's content, so a
/// section is padded the same above and below. Mirrors the layout of
/// `scan_view::bar_section` and `test_view::bar_section`.
fn section_header(label: &str, width: u16) -> Vec<Line<'static>> {
    let pad = (width as usize).saturating_sub(1);
    vec![
        Line::raw(""),
        Line::from(Span::styled(
            format!(" {label:<pad$}"),
            styles::Text::header(),
        )),
        Line::raw(""),
    ]
}

/// Type name for the tree: `gage::note` displays as `note`.
fn type_display(object_type: &str) -> String {
    object_type
        .strip_prefix("gage::")
        .unwrap_or(object_type)
        .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use gage_store::{NoteInput, NoteStore, NoteValue};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

    #[test]
    fn tree_expands_lazily_and_shows_blob_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let note_id = NoteStore::from(&store)
            .create(NoteInput {
                name: "n",
                value: NoteValue::Text("the note value".into()),
                author: "user:test",
                target: None,
            })
            .unwrap();

        let mut state = ViewState::new(store);
        state.reload();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();

        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("▼ Live"), "{text}");
        assert!(text.contains("  Deleted"), "empty group is a leaf: {text}");
        assert!(
            text.contains(&format!("  ▶ note {}", short_uuid(&note_id))),
            "{text}"
        );
        assert!(!text.contains("attrs.json"), "nothing expanded yet: {text}");
        assert!(text.contains("1 live object"), "group detail: {text}");

        // Expand the object: its commit tree appears, name-sorted
        let keys = state.tree.visible_keys();
        state.table.select_by(1, &keys);
        state.selection_changed();
        state.expand_selected();
        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("Object"),
            "object detail on the right: {text}"
        );
        let names: Vec<&str> = state
            .tree
            .rows()
            .iter()
            .skip(2)
            .filter_map(|r| match state.tree.data(r.node)? {
                Node::Entry(e) => Some(e.name.as_str()),
                Node::Group { .. } | Node::Object { .. } => None,
            })
            .collect();
        assert_eq!(
            names,
            [
                "attrs.json",
                "created",
                "id",
                "modified",
                "type",
                "value.txt"
            ]
        );
        assert!(text.contains("▼ note"), "{text}");

        // Select the `id` blob: its text is the note id
        let keys = state.tree.visible_keys();
        state.table.select_by(3, &keys);
        state.selection_changed();
        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains(&note_id), "blob text shown: {text}");

        // Select `attrs.json`: the compact blob is shown pretty-printed
        let keys = state.tree.visible_keys();
        state.table.select_by(-2, &keys);
        state.selection_changed();
        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("  \"author\": \"user:test\""), "{text}");

        // Collapse from a leaf moves to the parent; collapsing the
        // parent hides the entries, leaving the two groups and the
        // object
        state.collapse_selected();
        assert_eq!(state.table.selected_index(), Some(1));
        state.collapse_selected();
        assert_eq!(state.tree.rows().len(), 3);
    }

    #[test]
    fn deleted_objects_group_under_deleted_with_tombstone_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let notes = NoteStore::from(&store);
        let keep = notes
            .create(NoteInput {
                name: "keep",
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: None,
            })
            .unwrap();
        let gone = notes
            .create(NoteInput {
                name: "gone",
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: None,
            })
            .unwrap();
        notes.delete(&gone).unwrap();

        let mut state = ViewState::new(store);
        state.reload();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Objects (2)"), "{text}");
        let live_row = text.find("▼ Live").unwrap();
        let keep_row = text.find(&short_uuid(&keep).to_string()).unwrap();
        let deleted_row = text.find("▼ Deleted").unwrap();
        let gone_row = text.find(&short_uuid(&gone).to_string()).unwrap();
        assert!(
            live_row < keep_row && keep_row < deleted_row && deleted_row < gone_row,
            "{text}"
        );

        // Select the deleted object: the tombstone marker follows the
        // deleted timestamp on its own line
        let keys = state.tree.visible_keys();
        state.table.select_by(3, &keys);
        state.selection_changed();
        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        let deleted_line = text
            .lines()
            .find(|l| l.contains("deleted") && l.contains("(tombstone)"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            deleted_line.contains("T"),
            "timestamp on the same line: {deleted_line}"
        );
    }

    /// A driver whose sessions carry a `files.d/` subtree, so the
    /// listing table has a tree to show
    struct FakeDriver;

    struct FakeSession;

    struct FakeAttrs;

    impl gage_session::SessionAttrs for FakeAttrs {
        fn mtime(&self) -> Option<std::time::SystemTime> {
            None
        }
        fn size(&self) -> Option<u64> {
            Some(3)
        }
        fn is_empty(&self) -> Option<bool> {
            None
        }
        fn project_name(&self) -> Option<&str> {
            None
        }
        fn title(&self) -> Option<&str> {
            None
        }
        fn model(&self) -> Option<&str> {
            None
        }
        fn message_count(&self) -> Option<u64> {
            None
        }
    }

    impl gage_session::NativeSession for FakeSession {
        fn native_id(&self) -> &str {
            "s1"
        }
        fn session_type(&self) -> &str {
            "fake"
        }
        fn source(&self) -> &str {
            "fake:s1"
        }
        fn attrs(&self) -> &dyn gage_session::SessionAttrs {
            &FakeAttrs
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    impl gage_session::Driver for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn version(&self) -> &'static str {
            "0"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["fake"]
        }
        fn open_source(
            &self,
            _source: &str,
        ) -> Result<Box<dyn gage_session::Source>, gage_session::DriverError> {
            Err(gage_session::DriverError::Other("not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn gage_session::NativeSession,
            sink: &mut dyn gage_session::ContentSink,
        ) -> Result<String, gage_session::DriverError> {
            use std::io::Write as _;
            for (path, bytes) in [("b.txt", &b"two"[..]), ("a.txt", &b"one"[..])] {
                let mut w = sink.create(path).map_err(gage_session::DriverError::Io)?;
                w.write_all(bytes).map_err(gage_session::DriverError::Io)?;
            }
            Ok("fake-lines 1".to_string())
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn gage_session::ContentSource>,
        ) -> Result<Box<dyn gage_session::StoredSession>, gage_session::DriverError> {
            Err(gage_session::DriverError::Other("not used".into()))
        }
    }

    #[test]
    fn tree_listing_scrolls_through_its_own_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        gage_store::SessionStore::from(&store)
            .add(&FakeDriver, &mut FakeSession)
            .unwrap();

        let mut state = ViewState::new(store);
        state.reload();
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();

        // Expand the session: the tree lists first, then the blobs
        let keys = state.tree.visible_keys();
        state.table.select_by(1, &keys);
        state.selection_changed();
        state.expand_selected();
        let names: Vec<&str> = state
            .tree
            .rows()
            .iter()
            .skip(2)
            .filter_map(|r| match state.tree.data(r.node)? {
                Node::Entry(e) => Some(e.name.as_str()),
                Node::Group { .. } | Node::Object { .. } => None,
            })
            .collect();
        assert_eq!(
            names,
            ["files.d", "attrs.json", "created", "id", "modified", "type"]
        );
        let files_row = state
            .tree
            .rows()
            .iter()
            .position(
                |r| matches!(state.tree.data(r.node), Some(Node::Entry(e)) if e.name == "files.d"),
            )
            .unwrap();
        let keys = state.tree.visible_keys();
        let current = state.table.selected_index().unwrap();
        state
            .table
            .select_by(files_row as isize - current as isize, &keys);
        state.selection_changed();
        terminal.draw(|f| draw(f, &mut state)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("a.txt"), "{text}");
        assert!(text.contains("b.txt"), "{text}");
        assert_eq!(state.listing_table.selected_index(), Some(0));

        // Detail focus moves the listing selection, not the tree
        state.focus = Focus::Detail;
        let tree_before = state.table.selected_index();
        handle_key(&mut state, KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(state.listing_table.selected_index(), Some(1));
        assert_eq!(state.table.selected_index(), tree_before);
        handle_key(&mut state, KeyEvent::from(KeyCode::Char('G')));
        assert_eq!(state.listing_table.selected_index(), Some(1));
        handle_key(&mut state, KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(state.listing_table.selected_index(), Some(0));
    }

    #[test]
    fn tab_and_shift_tab_both_switch_panes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let mut state = ViewState::new(Store::open(&path).unwrap());
        assert_eq!(state.focus, Focus::Tree);
        handle_key(&mut state, KeyEvent::from(KeyCode::Tab));
        assert_eq!(state.focus, Focus::Detail);
        handle_key(&mut state, KeyEvent::from(KeyCode::BackTab));
        assert_eq!(state.focus, Focus::Tree);
        handle_key(&mut state, KeyEvent::from(KeyCode::BackTab));
        assert_eq!(state.focus, Focus::Detail);
        handle_key(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(state.focus, Focus::Tree);
    }

    #[test]
    fn nul_bytes_mark_a_blob_binary() {
        assert!(matches!(
            classify_blob(b"abc\0def".to_vec()),
            BlobContent::Binary
        ));
        assert!(matches!(
            classify_blob(vec![0xff, 0xfe]),
            BlobContent::Binary
        ));
        assert!(matches!(
            classify_blob(b"plain".to_vec()),
            BlobContent::Text(_)
        ));
    }
}
