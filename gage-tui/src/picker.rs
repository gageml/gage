//! Generic centered pick-list dialog: rows keyed by id, standard list
//! navigation (←/→ double as up/down so "previous/next then Enter" is
//! two keys), Enter opens, q/Esc closes. Rows render as a table under
//! host-defined columns; the picker resolves each column's display
//! width (fixed, content-fit, or fill) and ellipsizes overlong cells.
//! Hosts supply the columns and row cells and act on the returned
//! action. Used by the session, scan, and eval open dialogs.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Padding, Paragraph, Row, ScrollbarState, Table, TableState,
};
use unicode_width::UnicodeWidthStr;

use crate::item_table::scrollbar;
use crate::styles;
use crate::text::ellipsize;

pub(crate) struct Picker {
    title: &'static str,
    columns: Vec<PickColumn>,
    items: Vec<PickItem>,
    table: TableState,
    /// Visible rows, recorded at draw for paging keys
    viewport: u16,
}

pub(crate) struct PickColumn {
    heading: &'static str,
    width: PickWidth,
    align: Alignment,
}

enum PickWidth {
    Fixed(u16),
    /// Fit the widest cell, capped so one outlier cannot crowd the
    /// other columns. The heading always fits.
    Fit(u16),
    Fill,
}

impl PickColumn {
    /// Fixed-width left-aligned column.
    pub fn new(heading: &'static str, width: u16) -> Self {
        Self {
            heading,
            width: PickWidth::Fixed(width),
            align: Alignment::Left,
        }
    }

    /// Fixed-width right-aligned column.
    pub fn right(heading: &'static str, width: u16) -> Self {
        Self {
            heading,
            width: PickWidth::Fixed(width),
            align: Alignment::Right,
        }
    }

    /// Content-fit left-aligned column, capped at `cap` cells.
    pub fn fit(heading: &'static str, cap: u16) -> Self {
        Self {
            heading,
            width: PickWidth::Fit(cap),
            align: Alignment::Left,
        }
    }

    /// Column taking the remaining dialog width.
    pub fn fill(heading: &'static str) -> Self {
        Self {
            heading,
            width: PickWidth::Fill,
            align: Alignment::Left,
        }
    }
}

pub(crate) struct PickItem {
    pub id: String,
    /// One styled cell per configured column
    pub cells: Vec<Span<'static>>,
}

pub(crate) enum PickerAction {
    None,
    Close,
    Open(String),
}

const COLUMN_SPACING: u16 = 2;

impl Picker {
    /// New picker, preselecting `current` when it is in the list.
    pub fn new(
        title: &'static str,
        columns: Vec<PickColumn>,
        items: Vec<PickItem>,
        current: Option<&str>,
    ) -> Self {
        let mut table = TableState::default();
        let idx = current
            .and_then(|id| items.iter().position(|i| i.id == id))
            .unwrap_or(0);
        table.select((!items.is_empty()).then_some(idx));
        Self {
            title,
            columns,
            items,
            table,
            viewport: 0,
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> PickerAction {
        let page = (self.viewport.max(1)) as isize;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => PickerAction::Close,
            KeyCode::Enter => match self.table.selected().and_then(|i| self.items.get(i)) {
                Some(item) => PickerAction::Open(item.id.clone()),
                None => PickerAction::Close,
            },
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Right => {
                self.select_by(1);
                PickerAction::None
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Left => {
                self.select_by(-1);
                PickerAction::None
            }
            KeyCode::PageDown => {
                self.select_by(page);
                PickerAction::None
            }
            KeyCode::PageUp => {
                self.select_by(-page);
                PickerAction::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.table.select((!self.items.is_empty()).then_some(0));
                PickerAction::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.table.select(self.items.len().checked_sub(1));
                PickerAction::None
            }
            _ => PickerAction::None,
        }
    }

    fn select_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let max = (self.items.len() - 1) as isize;
        let current = self.table.selected().unwrap_or(0) as isize;
        self.table
            .select(Some(current.saturating_add(delta).clamp(0, max) as usize));
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let max_width = area.width.saturating_sub(8).clamp(40, 100);
        let width = self.content_width().map_or(max_width, |w| {
            // + block padding, borders, and the scrollbar column
            (w + 5).clamp(40, max_width)
        });
        // Keep at least a two-row margin above and below the dialog so
        // a long list never presses the frame against the screen edge.
        let height = (self.items.len() as u16 + 6).clamp(8, area.height.saturating_sub(4));
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(self.title, styles::Text::dim()))
            .border_style(styles::Text::dim())
            .padding(Padding::new(1, 1, 1, 0));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let [body_area, _gap, hint_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        // Reserve the rightmost column for the scrollbar so it never
        // covers table content — the pane tables get this shield from
        // their panel border.
        let [table_area, scrollbar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(body_area);
        // The header row occupies one line of the body
        self.viewport = table_area.height.saturating_sub(1);

        let widths = self.resolved_widths(table_area.width);
        let rows: Vec<Row<'static>> = self
            .items
            .iter()
            .map(|item| {
                let cells: Vec<Cell<'static>> = self
                    .columns
                    .iter()
                    .zip(&widths)
                    .zip(&item.cells)
                    .map(|((col, w), span)| cell(span.clone(), *w, col.align))
                    .collect();
                Row::new(cells)
            })
            .collect();
        let header = Row::new(self.columns.iter().zip(&widths).map(|(col, w)| {
            cell(
                Span::styled(col.heading, styles::Text::dim()),
                *w,
                col.align,
            )
        }));
        let constraints: Vec<Constraint> = widths.iter().map(|w| Constraint::Length(*w)).collect();
        let table = Table::new(rows, constraints)
            .header(header)
            .column_spacing(COLUMN_SPACING)
            .row_highlight_style(styles::Panel::selection(true));
        frame.render_stateful_widget(table, table_area, &mut self.table);

        let max_offset = self.items.len().saturating_sub(self.viewport as usize);
        let mut sb = ScrollbarState::new(max_offset).position(self.table.offset());
        frame.render_stateful_widget(scrollbar(true), scrollbar_area, &mut sb);

        let hint = Paragraph::new(Line::from(Span::styled(
            "Enter open · q cancel",
            styles::Text::dim(),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(hint, hint_area);
    }

    /// Column display widths for the given table width. Fixed and fit
    /// columns resolve from their own data; fill columns split the
    /// remainder.
    fn resolved_widths(&self, table_width: u16) -> Vec<u16> {
        let mut widths: Vec<Option<u16>> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| match col.width {
                PickWidth::Fixed(n) => Some(n),
                PickWidth::Fit(cap) => Some(self.fit_width(i, col.heading, cap)),
                PickWidth::Fill => None,
            })
            .collect();
        let fills = widths.iter().filter(|w| w.is_none()).count() as u16;
        let spacing = COLUMN_SPACING * (self.columns.len().saturating_sub(1)) as u16;
        let taken: u16 = widths.iter().flatten().sum();
        let remainder = table_width.saturating_sub(taken + spacing);
        if let Some(share) = remainder.checked_div(fills) {
            let mut extra = remainder % fills;
            for w in widths.iter_mut().filter(|w| w.is_none()) {
                let bonus = u16::from(extra > 0);
                extra -= bonus;
                *w = Some(share + bonus);
            }
        }
        widths.into_iter().flatten().collect()
    }

    /// Widest cell in column `i`, capped at `cap`; the heading always
    /// fits.
    fn fit_width(&self, i: usize, heading: &str, cap: u16) -> u16 {
        let content = self
            .items
            .iter()
            .filter_map(|item| item.cells.get(i))
            .map(|span| span.content.width() as u16)
            .max()
            .unwrap_or(0);
        content.min(cap).max(heading.width() as u16)
    }

    /// Content width when no column fills; a fill column takes the
    /// standard dialog width instead.
    fn content_width(&self) -> Option<u16> {
        let cols = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| match col.width {
                PickWidth::Fixed(n) => Some(n),
                PickWidth::Fit(cap) => Some(self.fit_width(i, col.heading, cap)),
                PickWidth::Fill => None,
            })
            .sum::<Option<u16>>()?;
        Some(cols + COLUMN_SPACING * (self.columns.len().saturating_sub(1)) as u16)
    }
}

/// A table cell: the span ellipsized to the column width, aligned per
/// the column.
fn cell(span: Span<'static>, width: u16, align: Alignment) -> Cell<'static> {
    let span = if span.content.width() > width as usize {
        Span::styled(ellipsize(&span.content, width as usize), span.style)
    } else {
        span
    };
    Cell::from(Line::from(span).alignment(align))
}

/// Relative age display for picker rows, e.g. `3h`.
pub(crate) fn ago(ms: i64) -> String {
    let secs = (gage_core::datetime::now_ms() - ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
