//! Attribute/value lines — the standard caption/value column layout
//! used by detail views (dim captions padded to a shared width, plain
//! values).

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::styles;

pub fn attr_lines(attrs: &[(&'static str, &str)]) -> Vec<Line<'static>> {
    let caption_width = attrs.iter().map(|(c, _)| c.width()).max().unwrap_or(0);
    attrs
        .iter()
        .map(|(caption, value)| {
            Line::from(vec![
                Span::styled(
                    format!("{caption:<width$}  ", width = caption_width),
                    styles::Text::dim(),
                ),
                Span::raw((*value).to_string()),
            ])
        })
        .collect()
}
