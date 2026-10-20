//! Footer key-help lines with the key characters highlighted.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::styles;

/// Build a footer help line from (keys, label) pairs, joined with
/// " · ". Key characters render in the key color; the `/` and space
/// separators between keys, the labels, and the joiners keep the
/// footer style.
pub(crate) fn help_line(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (keys, label)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        push_key_spans(&mut spans, keys, styles::Panel::footer_key());
        if !label.is_empty() {
            spans.push(Span::raw(format!(" {label}")));
        }
    }
    Line::from(spans)
}

/// Build a dialog key-hint line from (keys, label) pairs, joined with
/// " · ". Key characters render in the dialog key color; separators,
/// labels, and joiners stay the dialog hint style. This is the footer
/// convention applied to the dialog palette.
pub(crate) fn dialog_line(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (keys, label)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        push_key_spans(&mut spans, keys, styles::Dialog::key());
        if !label.is_empty() {
            spans.push(Span::raw(format!(" {label}")));
        }
    }
    Line::from(spans)
}

/// Split `keys` into key tokens and their `/`/space separators, e.g.
/// `[/]` yields `[`, `/`, `]`; only the tokens get `key_style`.
fn push_key_spans(spans: &mut Vec<Span<'static>>, keys: &str, key_style: Style) {
    let mut token = String::new();
    for c in keys.chars() {
        if c == '/' || c == ' ' {
            flush_token(spans, &mut token, key_style);
            spans.push(Span::raw(c.to_string()));
        } else {
            token.push(c);
        }
    }
    flush_token(spans, &mut token, key_style);
}

fn flush_token(spans: &mut Vec<Span<'static>>, token: &mut String, key_style: Style) {
    if !token.is_empty() {
        spans.push(Span::styled(std::mem::take(token), key_style));
    }
}

/// The newline shortcut shown in a text-entry dialog's key hint.
/// Shift+Enter is reported as a distinct key only under the Kitty
/// keyboard protocol; a legacy terminal sends the same bytes for Enter
/// and Shift+Enter, so the hint names the shortcuts legacy input
/// reports instead.
pub(crate) fn newline_keys(enhanced_keys: bool) -> &'static str {
    if enhanced_keys {
        "Shift+Enter"
    } else {
        "Alt+Enter or Ctrl+J"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_keys_names_shift_enter_only_under_the_kitty_protocol() {
        assert_eq!(newline_keys(true), "Shift+Enter");
        assert_eq!(newline_keys(false), "Alt+Enter or Ctrl+J");
    }
}
