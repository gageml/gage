//! Generic centered pick-list dialog: rows keyed by id, standard list
//! navigation (←/→ double as up/down so "previous/next then Enter" is
//! two keys), Enter opens, q/Esc closes. Hosts build the rows and act
//! on the returned action. Used by the session, scan, and eval open
//! dialogs.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, ScrollbarState};

use crate::item_table::scrollbar;
use crate::styles;

pub(crate) struct Picker {
    title: &'static str,
    items: Vec<PickItem>,
    list: ListState,
    /// Visible rows, recorded at draw for paging keys
    viewport: u16,
}

pub(crate) struct PickItem {
    pub id: String,
    pub line: Line<'static>,
}

pub(crate) enum PickerAction {
    None,
    Close,
    Open(String),
}

impl Picker {
    /// New picker, preselecting `current` when it is in the list.
    pub fn new(title: &'static str, items: Vec<PickItem>, current: Option<&str>) -> Self {
        let mut list = ListState::default();
        let idx = current
            .and_then(|id| items.iter().position(|i| i.id == id))
            .unwrap_or(0);
        list.select((!items.is_empty()).then_some(idx));
        Self {
            title,
            items,
            list,
            viewport: 0,
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> PickerAction {
        let page = (self.viewport.max(1)) as isize;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => PickerAction::Close,
            KeyCode::Enter => match self.list.selected().and_then(|i| self.items.get(i)) {
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
                self.list.select((!self.items.is_empty()).then_some(0));
                PickerAction::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.list.select(self.items.len().checked_sub(1));
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
        let current = self.list.selected().unwrap_or(0) as isize;
        self.list
            .select(Some(current.saturating_add(delta).clamp(0, max) as usize));
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.saturating_sub(8).clamp(40, 100);
        let height = (self.items.len() as u16 + 5).clamp(7, area.height.saturating_sub(2));
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
        self.viewport = body_area.height;

        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|item| ListItem::new(item.line.clone()))
            .collect();
        let list = List::new(items).highlight_style(styles::Panel::selection(true));
        frame.render_stateful_widget(list, body_area, &mut self.list);

        let max_offset = self.items.len().saturating_sub(self.viewport as usize);
        let mut sb = ScrollbarState::new(max_offset).position(self.list.offset());
        frame.render_stateful_widget(scrollbar(true), body_area, &mut sb);

        let hint = ratatui::widgets::Paragraph::new(Line::from(Span::styled(
            "Enter open · q cancel",
            styles::Text::dim(),
        )))
        .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(hint, hint_area);
    }
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
