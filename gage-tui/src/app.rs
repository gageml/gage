//! TUI shell — header / outline / body / footer. Tab toggles which pane is
//! active. In the outline: j/k/g/G/PgUp/PgDn navigate, Enter toggles
//! expansion, Right expands, Left collapses (or moves to parent when the
//! current row has no children to collapse). In the body: j/k/g/G/PgUp/PgDn
//! scroll. Entry rows render the raw JSON as syntax-highlighted YAML; the
//! session row shows session-level attributes.

use std::io;

use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use crate::doc::{Document, Entry};
use crate::options::ViewOptions;
use crate::outline::{CollapseOutcome, Outline, RowKind};
use crate::syntax::Highlighter;
use crate::{message, style};

pub fn run(
    terminal: &mut DefaultTerminal,
    doc: &Document,
    options: &ViewOptions,
) -> io::Result<()> {
    let turns = options.show_turns.then(|| compute_turns(doc));
    let mut state = AppState::new(doc);
    loop {
        terminal.draw(|frame| draw(frame, doc, &mut state, turns.as_deref()))?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Tab | KeyCode::BackTab => state.toggle_focus(),
                KeyCode::Down | KeyCode::Char('j') => match state.focus {
                    Focus::Outline => state.select_by(1),
                    Focus::Body => state.body_scroll_by(1),
                },
                KeyCode::Up | KeyCode::Char('k') => match state.focus {
                    Focus::Outline => state.select_by(-1),
                    Focus::Body => state.body_scroll_by(-1),
                },
                KeyCode::Char('g') => match state.focus {
                    Focus::Outline => state.select_first(),
                    Focus::Body => state.body_scroll_to_top(),
                },
                KeyCode::Char('G') => match state.focus {
                    Focus::Outline => state.select_last(),
                    Focus::Body => state.body_scroll_to_bottom(),
                },
                KeyCode::PageDown => match state.focus {
                    Focus::Outline => state.select_by(state.outline_page() as isize),
                    Focus::Body => state.body_scroll_by(state.body_page() as i32),
                },
                KeyCode::PageUp => match state.focus {
                    Focus::Outline => state.select_by(-(state.outline_page() as isize)),
                    Focus::Body => state.body_scroll_by(-(state.body_page() as i32)),
                },
                KeyCode::Enter if state.focus == Focus::Outline => state.toggle_selected(),
                KeyCode::Right if state.focus == Focus::Outline => state.expand_selected(),
                KeyCode::Left if state.focus == Focus::Outline => state.collapse_selected(),
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Outline,
    Body,
}

struct AppState {
    outline: Outline,
    list_state: ListState,
    focus: Focus,
    body_scroll: u16,
    /// Viewport metrics from the last draw. Input handlers read these to size
    /// page jumps and clamp scroll positions; the draw refreshes them and
    /// re-clamps so a resize that shrinks a viewport snaps back next frame.
    body_max_scroll: u16,
    body_viewport: u16,
    outline_viewport: u16,
    highlighter: Highlighter,
}

impl AppState {
    fn new(doc: &Document) -> Self {
        let outline = Outline::new(doc.entries.len());
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            outline,
            list_state,
            focus: Focus::Outline,
            body_scroll: 0,
            body_max_scroll: 0,
            body_viewport: 0,
            outline_viewport: 0,
            highlighter: Highlighter::new(),
        }
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

    /// Move selection by `delta` rows, clamped to `[0, len-1]`. Resets the
    /// body scroll because the body content is tied to the selection.
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

    /// Page size for paged navigation — 90% of the viewport, with a floor of
    /// 1 so very short viewports still advance.
    fn outline_page(&self) -> u16 {
        page_size(self.outline_viewport)
    }

    fn body_page(&self) -> u16 {
        page_size(self.body_viewport)
    }
}

fn page_size(viewport: u16) -> u16 {
    let v = u32::from(viewport);
    ((v * 9) / 10).max(1) as u16
}

fn draw(frame: &mut Frame, doc: &Document, state: &mut AppState, turns: Option<&[Option<usize>]>) {
    let [header_area, middle_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [outline_area, body_area] =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(0)]).areas(middle_area);

    let header =
        Paragraph::new(Line::from(format!("Session {}", doc.session.id))).style(style::header());
    frame.render_widget(header, header_area);

    draw_outline(frame, doc, state, outline_area, turns);
    draw_body(frame, doc, state, body_area);

    let footer = Paragraph::new(Line::from(
        "q quit · Tab switch pane · j/k g/G PgUp/PgDn · Enter ◂ ▸",
    ))
    .style(style::footer());
    frame.render_widget(footer, footer_area);
}

fn draw_outline(
    frame: &mut Frame,
    doc: &Document,
    state: &mut AppState,
    area: Rect,
    turns: Option<&[Option<usize>]>,
) {
    let active = state.focus == Focus::Outline;

    state.outline_viewport = area.height.saturating_sub(2);

    let selected = state.list_state.selected();
    let items: Vec<ListItem> = state
        .outline
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| row_to_item(row, doc, Some(i) == selected, turns))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(style::panel_border(active))
                .title("Entries"),
        )
        .highlight_style(style::selection());
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
    turns: Option<&[Option<usize>]>,
) -> ListItem<'static> {
    let indent = "  ".repeat(row.level.saturating_sub(1));
    let glyph = if row.has_children {
        if row.expanded { "▼ " } else { "▶ " }
    } else {
        "  "
    };
    let prefix = Span::raw(format!("{indent}{glyph}"));
    let line = match row.kind {
        RowKind::Session => Line::from(vec![prefix, Span::raw(doc.session.id.clone())]),
        RowKind::Entry { index } => {
            let kind = doc
                .entries
                .get(index)
                .map_or("?".to_string(), |e| e.label().to_string());
            // Dim on unselected rows only — the row's `reversed()` highlight
            // composes with `DIM` to a visibly different cell bg, so the
            // selected row drops the dim and inherits the plain reverse style.
            let number_style = if is_selected {
                Style::new()
            } else {
                style::text_dim()
            };
            let turn_suffix = turns
                .and_then(|t| t.get(index).copied().flatten())
                .map(|n| format!(" {}", circled_number(n)))
                .unwrap_or_default();
            Line::from(vec![
                prefix,
                Span::styled(format!("{} ", index + 1), number_style),
                Span::raw(format!("{kind}{turn_suffix}")),
            ])
        }
    };
    ListItem::new(line)
}

fn draw_body(frame: &mut Frame, doc: &Document, state: &mut AppState, area: Rect) {
    let active = state.focus == Focus::Body;
    let row = state
        .list_state
        .selected()
        .and_then(|i| state.outline.row(i));

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(style::panel_border(active))
        .title(body_title(doc, row.map(|r| &r.kind)));
    let inner = outer.inner(area);
    state.body_viewport = inner.height;
    frame.render_widget(outer, area);

    match row.map(|r| &r.kind) {
        Some(RowKind::Session) => {
            let lines = state.highlighter.highlight(&doc.session.yaml(), "yaml");
            draw_scrollable(frame, state, lines, inner);
        }
        Some(RowKind::Entry { index }) => match doc.entries.get(*index) {
            Some(entry) => draw_entry(frame, state, entry, inner),
            None => draw_placeholder(frame, "(missing entry)", inner),
        },
        None => draw_placeholder(frame, "(no selection)", inner),
    }

    draw_body_scrollbar(frame, state, active, area);
}

fn body_title(doc: &Document, kind: Option<&RowKind>) -> String {
    match kind {
        Some(RowKind::Session) => doc.session.id.clone(),
        Some(RowKind::Entry { index }) => {
            let kind = doc
                .entries
                .get(*index)
                .map_or("?".to_string(), |e| e.label().to_string());
            format!("{} {}", index + 1, kind)
        }
        None => "Body".to_string(),
    }
}

fn draw_entry(frame: &mut Frame, state: &mut AppState, entry: &Entry, area: Rect) {
    // Build the stack of widgets that compose the entry body. Each contributes
    // a height; we render them into an offscreen buffer at their stacked y
    // offsets, then blit the visible window into the frame.
    let mut sections: Vec<Section> = Vec::new();
    if let Some(message) = entry.message() {
        let panel = Paragraph::new(message::render(message))
            .wrap(Wrap { trim: false })
            .block(Block::default().padding(Padding::uniform(1)));
        sections.push(Section::from_paragraph(panel, area.width));
        sections.push(Section::from_paragraph(
            Paragraph::new(Line::from(Span::styled("--- raw ---", style::text_dim()))),
            area.width,
        ));
    }
    sections.push(Section::from_paragraph(
        Paragraph::new(state.highlighter.highlight(&entry.yaml(), "yaml"))
            .wrap(Wrap { trim: false }),
        area.width,
    ));

    draw_stack(frame, state, sections, area);
}

type RenderFn = Box<dyn FnOnce(Rect, &mut ratatui::buffer::Buffer)>;

/// A widget plus the height it needs at the current width. Held boxed so the
/// stack can mix paragraph variants under one type.
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
}

/// Renders `sections` stacked vertically into a virtual document, then blits
/// the `body_scroll`-offset slice into `area`. Updates scroll bounds.
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

    let dst = frame.buffer_mut();
    let scroll = state.body_scroll;
    for row in 0..area.height {
        let src_y = scroll.saturating_add(row);
        if src_y >= total {
            break;
        }
        for col in 0..area.width {
            if let (Some(src_cell), Some(dst_cell)) = (
                virt.cell(ratatui::layout::Position::new(col, src_y)),
                dst.cell_mut(ratatui::layout::Position::new(area.x + col, area.y + row)),
            ) {
                *dst_cell = src_cell.clone();
            }
        }
    }
}

fn draw_scrollable(frame: &mut Frame, state: &mut AppState, lines: Vec<Line<'static>>, area: Rect) {
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = u16::try_from(paragraph.line_count(area.width)).unwrap_or(u16::MAX);
    state.body_max_scroll = total.saturating_sub(area.height);
    if state.body_scroll > state.body_max_scroll {
        state.body_scroll = state.body_max_scroll;
    }
    frame.render_widget(paragraph.scroll((state.body_scroll, 0)), area);
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

/// One slot per entry; `Some(n)` on assistant entries, where `n` is the 1-based
/// turn index. A turn is one distinct assistant `message.id`; multiple entries
/// (thinking, text, tool_use) sharing that id all map to the same turn.
fn compute_turns(doc: &Document) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(doc.entries.len());
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut next: usize = 0;
    for entry in &doc.entries {
        let turn = if entry.entry_type() == "assistant" {
            entry
                .message()
                .and_then(|m| m.get("id"))
                .and_then(|v| v.as_str())
                .map(|id| match seen.get(id) {
                    Some(&n) => n,
                    None => {
                        next += 1;
                        seen.insert(id.to_string(), next);
                        next
                    }
                })
        } else {
            None
        };
        out.push(turn);
    }
    out
}

/// Render `n` as circled-digit unicode. 1..=20 use a single glyph (U+2460..);
/// past 20, digits compose individually (⓪..⑨), so 21 renders as ②①.
fn circled_number(n: usize) -> String {
    if (1..=20).contains(&n) {
        return char::from_u32(0x2460 + (n as u32) - 1)
            .expect("U+2460..=U+2473 are valid scalars")
            .to_string();
    }
    n.to_string().chars().map(circled_digit).collect()
}

fn circled_digit(d: char) -> char {
    match d {
        '0' => '\u{24EA}',
        '1'..='9' => char::from_u32(0x2460 + (d as u32 - '1' as u32))
            .expect("U+2460..=U+2468 are valid scalars"),
        _ => d,
    }
}

fn scrollbar(active: bool) -> Scrollbar<'static> {
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("┃")
        .track_symbol(Some("│"))
        .style(style::scrollbar(active))
}
