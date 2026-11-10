//! Attribute/value lines — the standard caption/value column layout
//! used by detail views (dim captions padded to a shared width, plain
//! values). Every caption/value listing in a body pane goes through
//! [`attr_lines`] or [`styled_attr_lines`] so the listings read alike.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::styles;

/// Caption/value rows with plain values.
pub fn attr_lines(attrs: &[(&str, &str)]) -> Vec<Line<'static>> {
    let styled: Vec<(&str, &str, Style)> = attrs
        .iter()
        .map(|(caption, value)| (*caption, *value, Style::new()))
        .collect();
    styled_attr_lines(&styled)
}

/// Caption/value rows, each value in its own style. The captions are
/// dim and padded to the widest caption plus two spaces, so the values
/// share a column; a value's later lines are indented to that column.
pub fn styled_attr_lines(attrs: &[(&str, &str, Style)]) -> Vec<Line<'static>> {
    let caption_width = attrs.iter().map(|(c, _, _)| c.width()).max().unwrap_or(0);
    let indent = " ".repeat(caption_width + 2);
    let mut lines = Vec::new();
    for (caption, value, style) in attrs {
        let mut value_lines = value.lines();
        let first = value_lines.next().unwrap_or("");
        lines.push(Line::from(vec![
            Span::styled(
                format!("{caption:<width$}  ", width = caption_width),
                styles::Text::dim(),
            ),
            Span::styled(first.to_string(), *style),
        ]));
        for cont in value_lines {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(cont.to_string(), *style),
            ]));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Captions pad to the widest plus two spaces, a value's later
    /// lines indent to the value column, and a styled value keeps its
    /// style on every line while the caption stays dim.
    #[test]
    fn captions_share_a_column_and_values_keep_their_style() {
        let red = Style::new().fg(Color::Red);
        let lines = styled_attr_lines(&[
            ("Tool", "Bash", Style::new()),
            ("Status", "error\nagain", red),
        ]);
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(texts, ["Tool    Bash", "Status  error", "        again"]);
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Red));
        assert_eq!(text(&attr_lines(&[("Tool", "Bash")])[0]), "Tool  Bash");
    }
}
