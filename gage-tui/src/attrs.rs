//! Attribute/value lines — the standard caption/value column layout
//! used by detail views (dim captions padded to a shared width, plain
//! values). Every caption/value listing in a body pane goes through
//! [`attr_lines`] or [`styled_attr_lines`] so the listings read alike.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::styles;
use crate::textarea::wrap_line;

/// Caption/value rows with plain values, fitted to `width` columns.
pub fn attr_lines(attrs: &[(&str, &str)], width: u16) -> Vec<Line<'static>> {
    let styled: Vec<(&str, &str, Style)> = attrs
        .iter()
        .map(|(caption, value)| (*caption, *value, Style::new()))
        .collect();
    styled_attr_lines(&styled, width)
}

/// The widest caption that shares a line with its value.
const MAX_CAPTION_WIDTH: usize = 24;

/// Caption/value rows, each value in its own style. The captions are
/// dim and padded to the widest caption plus two spaces, so the values
/// share a column. A caption wider than [`MAX_CAPTION_WIDTH`] takes no
/// part in that column: it has its own line and its value starts on
/// the next. A value line longer than the columns left of `width` is
/// wrapped, and every row after a caption is indented to the value
/// column.
pub fn styled_attr_lines(attrs: &[(&str, &str, Style)], width: u16) -> Vec<Line<'static>> {
    let caption_width = attrs
        .iter()
        .map(|(c, _, _)| c.width())
        .filter(|w| *w <= MAX_CAPTION_WIDTH)
        .max()
        .unwrap_or(0);
    let indent = " ".repeat(caption_width + 2);
    let value_width = usize::from(width).saturating_sub(caption_width + 2);
    let mut lines = Vec::new();
    for (caption, value, style) in attrs {
        let mut lead = if caption.width() > MAX_CAPTION_WIDTH {
            lines.push(Line::from(Span::styled(
                caption.to_string(),
                styles::Text::dim(),
            )));
            Span::raw(indent.clone())
        } else {
            Span::styled(
                format!("{caption:<width$}  ", width = caption_width),
                styles::Text::dim(),
            )
        };
        let mut rows = value.lines().flat_map(|line| value_rows(line, value_width));
        let first = rows.next().unwrap_or("");
        for row in std::iter::once(first).chain(rows) {
            lines.push(Line::from(vec![
                lead,
                Span::styled(row.to_string(), *style),
            ]));
            lead = Span::raw(indent.clone());
        }
    }
    lines
}

/// One value line as the rows that fit `width` columns. A line that
/// fits, an empty line, and any line when no columns are left is one
/// row. The space a row breaks at counts toward its width and is not
/// shown.
fn value_rows(line: &str, width: usize) -> Vec<&str> {
    let rows = wrap_line(line, u16::try_from(width).unwrap_or(u16::MAX));
    if rows.is_empty() {
        return vec![line];
    }
    rows.into_iter()
        .map(|range| line[range].trim_end())
        .collect()
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
        let lines = styled_attr_lines(
            &[
                ("Tool", "Bash", Style::new()),
                ("Status", "error\nagain", red),
            ],
            80,
        );
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(texts, ["Tool    Bash", "Status  error", "        again"]);
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Red));
        assert_eq!(text(&attr_lines(&[("Tool", "Bash")], 80)[0]), "Tool  Bash");
    }

    /// A value longer than the columns beside the caption wraps at a
    /// word boundary, a word longer than those columns is broken, and
    /// every wrapped row is indented to the value column and keeps the
    /// value's style.
    #[test]
    fn long_values_wrap_within_the_value_column() {
        let red = Style::new().fg(Color::Red);
        let lines = styled_attr_lines(
            &[
                ("File", "The user opened the file", Style::new()),
                ("Id", "abcdefghijklmnop\nxy", red),
            ],
            16,
        );
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(
            texts,
            [
                "File  The user",
                "      opened",
                "      the file",
                "Id    abcdefghij",
                "      klmnop",
                "      xy",
            ]
        );
        assert!(texts.iter().all(|t| t.width() <= 16), "{texts:?}");
        assert_eq!(lines[4].spans[1].style.fg, Some(Color::Red));
    }

    /// A caption wider than the maximum has its own line, its value
    /// starts on the next line in the value column, and it does not
    /// widen that column.
    #[test]
    fn a_caption_over_the_maximum_has_its_own_line() {
        let lines = attr_lines(
            &[
                ("messageId", "m1"),
                ("gage-rscan/src/a_long_runner.rs", "v8\nv9"),
            ],
            80,
        );
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(
            texts,
            [
                "messageId  m1",
                "gage-rscan/src/a_long_runner.rs",
                "           v8",
                "           v9",
            ]
        );
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    /// A width with no columns left for values leaves each value line
    /// whole.
    #[test]
    fn a_width_narrower_than_the_captions_does_not_wrap() {
        let lines = attr_lines(&[("Caption", "a long value")], 4);
        assert_eq!(text(&lines[0]), "Caption  a long value");
        assert_eq!(lines.len(), 1);
    }
}
