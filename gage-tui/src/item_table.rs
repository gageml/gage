//! Identity-stable table view state.
//!
//! Ratatui's `TableState` is positional — an index and a scroll
//! offset. `ItemTable` layers the view-state model a selection list
//! needs over data that reorders or gets replaced: the selection's
//! source of truth is the item *id*, and the positional `TableState`
//! is a derived projection. Callers pass the display-order id list to
//! every operation; after the data changes, [`ItemTable::update`]
//! re-derives the index from the selected id (falling back to the old
//! position when the item is gone).

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, Table, TableState};

use crate::style;

#[derive(Default)]
pub(crate) struct ItemTable {
    /// Selection mode. `None` is "unpinned": the first row is
    /// highlighted and the highlight follows whatever item is first as
    /// the data reorders. `Some(id)` is "pinned": the user navigated to
    /// an item and selection sticks to it across reorders.
    selected: Option<String>,
    /// Derived positional state fed to ratatui
    table: TableState,
    /// Visible row count, recorded at render time so paging keys know
    /// the page size before the next frame
    viewport: usize,
}

impl ItemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile with a changed id list. Unpinned: highlight the first
    /// row. Pinned: follow the id; when the item is gone, re-pin to the
    /// previous position (clamped).
    pub fn update(&mut self, ids: &[&str]) {
        match self.selected.as_deref() {
            None => self.table.select((!ids.is_empty()).then_some(0)),
            Some(sel) => {
                let index = ids.iter().position(|id| *id == sel).or_else(|| {
                    let fallback = self.table.selected().unwrap_or(0);
                    (!ids.is_empty()).then(|| fallback.min(ids.len() - 1))
                });
                self.pin(index, ids);
            }
        }
    }

    /// Move the selection, pinning it to the item landed on.
    pub fn select_by(&mut self, delta: isize, ids: &[&str]) {
        if ids.is_empty() {
            self.pin(None, ids);
            return;
        }
        let next = match self.table.selected() {
            Some(current) => (current as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
            None => 0,
        };
        self.pin(Some(next), ids);
    }

    /// Go to the first row and resume following it (unpin).
    pub fn select_first(&mut self, ids: &[&str]) {
        self.selected = None;
        self.table.select((!ids.is_empty()).then_some(0));
    }

    pub fn select_last(&mut self, ids: &[&str]) {
        self.pin(ids.len().checked_sub(1), ids);
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.table.selected()
    }

    /// Page size for paging keys — the last rendered viewport
    pub fn page(&self) -> usize {
        self.viewport.max(1)
    }

    /// Render the table plus its scrollbar, recording the viewport.
    /// The viewport excludes the panel borders and the header row.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        table: Table,
        len: usize,
        active: bool,
    ) {
        self.viewport = area.height.saturating_sub(3) as usize;
        frame.render_stateful_widget(table, area, &mut self.table);
        let max_offset = len.saturating_sub(self.viewport);
        let mut sb_state = ScrollbarState::new(max_offset).position(self.table.offset());
        frame.render_stateful_widget(
            scrollbar(active),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }

    fn pin(&mut self, index: Option<usize>, ids: &[&str]) {
        self.table.select(index);
        self.selected = index.and_then(|i| ids.get(i)).map(|id| (*id).to_string());
    }
}

pub(crate) fn scrollbar(active: bool) -> Scrollbar<'static> {
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .thumb_symbol("┃")
        .track_symbol(Some("│"))
        .style(style::scrollbar(active))
}
