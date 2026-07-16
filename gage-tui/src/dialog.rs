//! Message dialog — a small centered modal for announcements and
//! confirmations.
//!
//! Rendered in reverse video so it stands out from the view beneath
//! it; larger content modals (detail/log views) keep the normal
//! palette since they replace the screen rather than interrupt it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::styles;

/// Draw a centered one-line message with a key hint below it.
pub(crate) fn draw_message(frame: &mut Frame, message: &str, hint: &str) {
    draw_lines(frame, vec![Line::raw(message.to_string())], hint);
}

/// Draw centered content lines with a key-hint footer below the
/// dialog border. The dialog is sized to fit the widest line.
pub(crate) fn draw_lines(frame: &mut Frame, lines: Vec<Line>, hint: &str) {
    let frame_area = frame.area();
    let content_width = lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(0)
        .max(hint.width())
        + 6;
    let width = (content_width as u16).min(frame_area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 5).min(frame_area.height.saturating_sub(2));
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    // The full area is the dialog surface; the border stops one row
    // short so the hint sits on the surface below it.
    frame.render_widget(Block::new().style(styles::Dialog::surface()), area);
    let [border_area, hint_row] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let block = Block::bordered();
    let inner = block.inner(border_area);
    frame.render_widget(block, border_area);
    let [_, content, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(lines.len() as u16),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(lines).centered(), content);
    frame.render_widget(
        Paragraph::new(Span::styled(hint.to_string(), styles::Dialog::dim())).centered(),
        hint_row,
    );
}
