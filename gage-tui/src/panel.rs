//! Shared panel chrome: the block-and-title box every view uses for
//! its named regions, and the dim header row every table above sits
//! under. Kept in one place so a chrome change is a one-file edit.

use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Row};

use crate::styles;

/// Bordered panel with `title`, styled to reflect focus. Match the
/// convention used across every view: title text is plain; only the
/// border tone changes with focus, so the title reads the same when
/// the panel is inactive.
pub(crate) fn panel_block(title: String, active: bool) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(styles::Panel::border(active))
}

/// Dim header row for a table with fixed columns. Each cell is a
/// [`Span::styled`] with [`styles::Text::dim`].
pub(crate) fn header_row<const N: usize>(names: [&'static str; N]) -> Row<'static> {
    Row::new(names.map(|n| Cell::from(Span::styled(n, styles::Text::dim()))))
}
