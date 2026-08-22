//! Footer key-help lines with the key characters highlighted.

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
        push_key_spans(&mut spans, keys);
        if !label.is_empty() {
            spans.push(Span::raw(format!(" {label}")));
        }
    }
    Line::from(spans)
}

/// Split `keys` into key tokens and their `/`/space separators, e.g.
/// `[/]` yields `[`, `/`, `]`; only the tokens get the key color.
fn push_key_spans(spans: &mut Vec<Span<'static>>, keys: &str) {
    let mut token = String::new();
    for c in keys.chars() {
        if c == '/' || c == ' ' {
            flush_token(spans, &mut token);
            spans.push(Span::raw(c.to_string()));
        } else {
            token.push(c);
        }
    }
    flush_token(spans, &mut token);
}

fn flush_token(spans: &mut Vec<Span<'static>>, token: &mut String) {
    if !token.is_empty() {
        spans.push(Span::styled(
            std::mem::take(token),
            styles::Panel::footer_key(),
        ));
    }
}
