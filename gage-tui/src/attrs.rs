//! Attribute/value lines — the standard caption/value column layout
//! used by detail views (dim captions padded to a shared width, plain
//! values).

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::styles;

pub fn attr_lines(attrs: &[(&'static str, &str)]) -> Vec<Line<'static>> {
    let caption_width = attrs.iter().map(|(c, _)| c.width()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (caption, value) in attrs {
        let mut value_lines = value.lines();
        let first = value_lines.next().unwrap_or("");
        lines.push(Line::from(vec![
            Span::styled(
                format!("{caption:<width$}  ", width = caption_width),
                styles::Text::dim(),
            ),
            Span::raw(first.to_string()),
        ]));
        let indent = " ".repeat(caption_width + 2);
        for cont in value_lines {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::raw(cont.to_string()),
            ]));
        }
    }
    lines
}
