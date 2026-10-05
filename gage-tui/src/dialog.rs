//! Message dialog — a small centered modal for announcements and
//! confirmations.
//!
//! Rendered in reverse video so it stands out from the view beneath
//! it; larger content modals (detail/log views) keep the normal
//! palette since they replace the screen rather than interrupt it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, ScrollbarState, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::item_table::scrollbar;
use crate::styles;
use crate::textarea::TextArea;

/// Rows of editor text an editor dialog shows; the text scrolls
/// beyond them
const EDITOR_ROWS: u16 = 8;

/// Draw a centered one-line message with a key hint below it.
pub(crate) fn draw_message(frame: &mut Frame, message: &str, hint: &str) {
    draw_lines(frame, vec![Line::raw(message.to_string())], hint);
}

/// Draw a centered dialog whose text wraps to a readable width,
/// followed by a blank line and left-aligned option rows. The rows
/// carry their keys inline, so there is no key-hint footer.
pub(crate) fn draw_wrapped_options(
    frame: &mut Frame,
    paragraphs: &[&str],
    options: Vec<Line<'static>>,
) {
    let frame_area = frame.area();
    let text_width = frame_area.width.saturating_sub(8).clamp(20, 52);
    let mut lines: Vec<Line> = Vec::new();
    for (i, p) in paragraphs.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::raw(p.to_string()));
    }
    if !options.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(options);
    }
    // trim: false keeps the option rows' leading indent
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let content_height = (para.line_count(text_width) as u16).max(1);
    let width = (text_width + 6).min(frame_area.width.saturating_sub(2));
    let height = (content_height + 4).min(frame_area.height.saturating_sub(2));
    let area = Rect {
        x: frame_area.x + (frame_area.width.saturating_sub(width)) / 2,
        y: frame_area.y + (frame_area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Block::new().style(styles::Dialog::surface()), area);
    let block = Block::bordered();
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [_, content_rows, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(content_height),
        Constraint::Length(1),
    ])
    .areas(inner);
    let [_, content, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(text_width),
        Constraint::Fill(1),
    ])
    .areas(content_rows);
    frame.render_widget(para, content);
}

/// Draw a centered message that wraps to a readable width, with a
/// key-hint footer below the dialog border.
pub(crate) fn draw_wrapped_message(frame: &mut Frame, message: &str, hint: &str) {
    let frame_area = frame.area();
    let text_width = (message.width() as u16)
        .min(frame_area.width.saturating_sub(8).clamp(20, 52))
        .max(hint.width() as u16);
    let para = Paragraph::new(message.to_string())
        .wrap(Wrap { trim: true })
        .centered();
    let content_height = (para.line_count(text_width) as u16).max(1);
    let content = draw_chrome(frame, None, text_width, content_height, hint);
    let [_, text, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(text_width),
        Constraint::Fill(1),
    ])
    .areas(content);
    frame.render_widget(para, text);
}

/// Draw a text editor dialog: an optional one-line prompt above
/// the editor body inside a titled border, with a key-hint footer
/// below the border. A scrollbar appears once the text outgrows the
/// body.
pub(crate) fn draw_editor(
    frame: &mut Frame,
    title: &str,
    prompt: Option<&str>,
    editor: &mut TextArea,
    hint: &str,
) {
    let frame_area = frame.area();
    let text_width = frame_area.width.saturating_sub(8).clamp(30, 72);
    let prompt_rows = u16::from(prompt.is_some());
    let content = draw_chrome(
        frame,
        Some(title),
        text_width,
        EDITOR_ROWS + prompt_rows,
        hint,
    );
    let [_, column, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(text_width),
        Constraint::Fill(1),
    ])
    .areas(content);
    let [prompt_area, body] =
        Layout::vertical([Constraint::Length(prompt_rows), Constraint::Fill(1)]).areas(column);
    if let Some(prompt) = prompt {
        frame.render_widget(
            Paragraph::new(Span::styled(prompt.to_string(), styles::Dialog::dim())),
            prompt_area,
        );
    }
    // Wrap is measured against the body less the scrollbar column.
    // The bar appears only once the text exceeds the body, so the
    // narrower width is the one that matters.
    let probe_width = body.width.saturating_sub(1).max(1);
    let needs_bar = editor.visual_row_count(probe_width) > body.height as usize;
    let (text_area, bar_area) = if needs_bar {
        let [t, b] = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(body);
        (t, Some(b))
    } else {
        (body, None)
    };
    if let Some((x, y)) = editor.render(text_area, frame.buffer_mut(), Style::default()) {
        frame.set_cursor_position((x, y));
    }
    if let Some(bar_area) = bar_area {
        let mut state = ScrollbarState::new(editor.visual_row_count(text_area.width))
            .viewport_content_length(text_area.height as usize)
            .position(editor.visual_cursor_row(text_area.width));
        frame.render_stateful_widget(scrollbar(true), bar_area, &mut state);
    }
}

/// Draw centered content lines with a key-hint footer below the
/// dialog border. The dialog is sized to fit the widest line.
pub(crate) fn draw_lines(frame: &mut Frame, lines: Vec<Line>, hint: &str) {
    draw_lines_titled(frame, None, lines, hint);
}

/// [`draw_lines`] with an optional title in the border.
pub(crate) fn draw_lines_titled(
    frame: &mut Frame,
    title: Option<&str>,
    lines: Vec<Line>,
    hint: &str,
) {
    let text_width = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let content = draw_chrome(frame, title, text_width, lines.len() as u16, hint);
    frame.render_widget(Paragraph::new(lines).centered(), content);
}

/// Draw the dialog surface, border, and hint row for content
/// `text_width` wide and `content_height` tall, centered on the
/// frame, and return the content area: the border's interior less
/// one row of padding above and below. The interior is four columns
/// wider than the text, two each side, and is clipped to the frame.
fn draw_chrome(
    frame: &mut Frame,
    title: Option<&str>,
    text_width: u16,
    content_height: u16,
    hint: &str,
) -> Rect {
    let frame_area = frame.area();
    let width = (text_width.max(hint.width() as u16) + 6).min(frame_area.width.saturating_sub(2));
    let height = (content_height + 5).min(frame_area.height.saturating_sub(2));
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
    let mut block = Block::bordered();
    if let Some(title) = title {
        block = block.title(Span::styled(title.to_string(), styles::Dialog::dim()));
    }
    let inner = block.inner(border_area);
    frame.render_widget(block, border_area);
    let [_, content, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(content_height),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(Span::styled(hint.to_string(), styles::Dialog::dim())).centered(),
        hint_row,
    );
    content
}
