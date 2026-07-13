//! Message dialog — a small centered modal for announcements and
//! confirmations.
//!
//! Rendered in reverse video so it stands out from the view beneath
//! it; larger content modals (detail/log views) keep the normal
//! palette since they replace the screen rather than interrupt it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::style;

/// Draw a centered one-line message with a key hint below it.
pub(crate) fn draw_message(frame: &mut Frame, message: &str, hint: &str) {
    let frame_area = frame.area();
    let content_width = message.width().max(hint.width()) + 6;
    let width = (content_width as u16).min(frame_area.width.saturating_sub(2));
    let height = 6u16.min(frame_area.height.saturating_sub(2));
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let block = Block::bordered().style(style::dialog());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [_, msg, _, hint_row] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(message.to_string()).centered(), msg);
    frame.render_widget(
        Paragraph::new(Span::styled(hint.to_string(), style::text_dim())).centered(),
        hint_row,
    );
}
