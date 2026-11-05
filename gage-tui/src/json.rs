//! A JSON value painted as rows for a pane. The structure is in
//! serde_json's pretty layout with each token styled by its kind. A
//! string's text is shown as text: a line break in it starts a new
//! row, and text that reaches the pane edge continues on the next
//! row. Both continue one indent level past the row that holds the
//! value. A row broken for width ends with a dim `↵`; a row that ends
//! at a line break in the string has no marker. The last column of
//! the pane is reserved for the marker. The rows are final; the pane
//! paints them without wrapping.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::styles;

const INDENT: usize = 2;
const TAB: &str = "    ";
/// Ends a row that was broken for width
const WRAP: &str = "↵";

/// The rows of `value` for a pane `width` cells wide. A width of zero
/// means no wrapping.
pub(crate) fn render(value: &Value, width: u16) -> Vec<Line<'static>> {
    let mut rows = Rows::new(width);
    rows.value(value, 0);
    rows.finish()
}

struct Rows {
    /// Cells a row may hold; the pane's last column is the marker's
    width: usize,
    done: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    /// Cells painted on the row so far
    col: usize,
    /// Column a continuation of the row starts at
    hang: usize,
}

impl Rows {
    fn new(width: u16) -> Self {
        Self {
            width: if width == 0 {
                usize::MAX
            } else {
                usize::from(width).saturating_sub(1)
            },
            done: Vec::new(),
            spans: Vec::new(),
            col: 0,
            hang: 0,
        }
    }

    fn value(&mut self, value: &Value, indent: usize) {
        let plain = Style::new();
        match value {
            Value::Null => self.token("null", styles::Syntax::constant()),
            Value::Bool(true) => self.token("true", styles::Syntax::constant()),
            Value::Bool(false) => self.token("false", styles::Syntax::constant()),
            Value::Number(n) => self.token(&n.to_string(), styles::Syntax::number()),
            Value::String(s) => self.string(s, indent),
            Value::Array(items) if items.is_empty() => self.token("[]", plain),
            Value::Array(items) => {
                self.token("[", plain);
                let last = items.len() - 1;
                for (i, item) in items.iter().enumerate() {
                    self.newline(indent + INDENT);
                    self.value(item, indent + INDENT);
                    if i < last {
                        self.token(",", plain);
                    }
                }
                self.newline(indent);
                self.token("]", plain);
            }
            Value::Object(map) if map.is_empty() => self.token("{}", plain),
            Value::Object(map) => {
                self.token("{", plain);
                let last = map.len() - 1;
                for (i, (key, item)) in map.iter().enumerate() {
                    self.newline(indent + INDENT);
                    self.token(&format!("\"{key}\""), styles::Syntax::key());
                    self.token(": ", plain);
                    self.value(item, indent + INDENT);
                    if i < last {
                        self.token(",", plain);
                    }
                }
                self.newline(indent);
                self.token("}", plain);
            }
        }
    }

    /// A token that stays whole. It moves to a continuation row when
    /// it does not fit and the row already holds something past the
    /// hang column.
    fn token(&mut self, text: &str, style: Style) {
        let width = text.width();
        if self.col + width > self.width && self.col > self.hang {
            self.wrap();
        }
        self.spans.push(Span::styled(text.to_string(), style));
        self.col += width;
    }

    /// A string literal on a row at `indent`. Its text continues,
    /// after a line break or at the pane edge, one indent level past
    /// `indent`. A grapheme is never split; one wider than the room
    /// left on an otherwise empty continuation row overruns it.
    fn string(&mut self, text: &str, indent: usize) {
        let style = styles::Syntax::string();
        self.token("\"", style);
        self.hang = indent + INDENT;
        let text = text.replace('\t', TAB);
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline(self.hang);
            }
            let mut run = String::new();
            for grapheme in line.graphemes(true) {
                let width = grapheme.width();
                if self.col + width > self.width && self.col > self.hang {
                    self.spans
                        .push(Span::styled(std::mem::take(&mut run), style));
                    self.wrap();
                }
                run.push_str(grapheme);
                self.col += width;
            }
            if !run.is_empty() {
                self.spans.push(Span::styled(run, style));
            }
        }
        self.token("\"", style);
    }

    /// Ends a row broken for width with the marker and continues at
    /// the hang column.
    fn wrap(&mut self) {
        self.spans.push(Span::styled(WRAP, styles::Text::dim()));
        self.newline(self.hang);
    }

    /// Ends the row and starts the next at `col`.
    fn newline(&mut self, col: usize) {
        let spans = std::mem::take(&mut self.spans);
        self.done.push(Line::from(spans));
        if col > 0 {
            self.spans.push(Span::raw(" ".repeat(col)));
        }
        self.col = col;
        self.hang = col;
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.done.push(Line::from(self.spans));
        self.done
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn texts(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter().map(|l| l.to_string()).collect()
    }

    /// Without line breaks or long strings the rows are serde_json's
    /// pretty layout, token for token.
    #[test]
    fn layout_is_serde_json_pretty_layout() {
        let value = json!({
            "a": [1, 2.5, -3, true, false, null, "x y"],
            "b": {"c": "d", "e": {}},
            "f": [],
            "g": ""
        });
        let rows = render(&value, 80);
        let expected: Vec<String> = serde_json::to_string_pretty(&value)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(texts(&rows), expected);
    }

    /// A line break in a string starts a new row one indent level
    /// past the key's row, with no marker; an empty line keeps the
    /// indent.
    #[test]
    fn a_line_break_in_a_string_continues_one_level_in_without_a_marker() {
        let rows = render(&json!({"k": "a\nbb\n\nc", "n": 1}), 80);
        assert_eq!(
            texts(&rows),
            [
                "{",
                "  \"k\": \"a",
                "    bb",
                "    ",
                "    c\",",
                "  \"n\": 1",
                "}",
            ]
        );
        assert!(rows.iter().all(|r| !r.to_string().contains(WRAP)));
    }

    /// A string that reaches the pane edge ends the row with the dim
    /// marker in the last column and continues one indent level past
    /// the row that holds it; a closing quote or comma that would not
    /// fit breaks the same way.
    #[test]
    fn a_width_break_is_marked_and_continues_one_level_in() {
        let rows = render(&json!({"k": "0123456789abcdefghij", "n": 1}), 20);
        assert_eq!(
            texts(&rows),
            [
                "{",
                "  \"k\": \"0123456789a↵",
                "    bcdefghij\",",
                "  \"n\": 1",
                "}",
            ]
        );
        let marker = rows[1].spans.last().unwrap();
        assert_eq!(marker.content, WRAP);
        assert_eq!(marker.style, styles::Text::dim());

        let rows = render(&json!(["0123456789abcdefg", 1]), 20);
        assert_eq!(
            texts(&rows),
            ["[", "  \"0123456789abcdef↵", "    g\",", "  1", "]"]
        );
        let rows = render(&json!(["0123456789abcdef", 1]), 20);
        assert_eq!(
            texts(&rows),
            ["[", "  \"0123456789abcdef↵", "    \",", "  1", "]"]
        );
    }

    /// A wide character counts two cells and is not split across rows.
    #[test]
    fn wide_characters_wrap_whole() {
        let rows = render(&json!({"k": "日本語"}), 11);
        assert_eq!(texts(&rows), ["{", "  \"k\": \"日↵", "    本語\"", "}"]);
    }

    /// Tabs show as four spaces.
    #[test]
    fn tabs_expand_to_four_spaces() {
        let rows = render(&json!("a\tb"), 80);
        assert_eq!(texts(&rows), ["\"a    b\""]);
    }

    /// Keys, strings, numbers, and constants carry their styles;
    /// punctuation and padding carry none.
    #[test]
    fn tokens_carry_the_style_of_their_kind() {
        let rows = render(&json!({"k": ["s", 1, true, null]}), 80);
        let style_of = |text: &str| -> Style {
            rows.iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content == text)
                .unwrap_or_else(|| panic!("no span {text:?}"))
                .style
        };
        assert_eq!(style_of("\"k\""), styles::Syntax::key());
        assert_eq!(style_of("\""), styles::Syntax::string());
        assert_eq!(style_of("s"), styles::Syntax::string());
        assert_eq!(style_of("1"), styles::Syntax::number());
        assert_eq!(style_of("true"), styles::Syntax::constant());
        assert_eq!(style_of("null"), styles::Syntax::constant());
        assert_eq!(style_of(": "), Style::new());
        assert_eq!(style_of("    "), Style::new());
    }

    /// Width zero means no wrapping.
    #[test]
    fn width_zero_never_wraps() {
        let long = "x".repeat(500);
        let rows = render(&json!({"k": long}), 0);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].to_string().len(), 2 + 5 + 1 + 500 + 1);
    }
}
