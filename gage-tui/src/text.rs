//! Text display helpers shared across views.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate to `width` cells, marking the cut with a trailing ellipsis.
pub(crate) fn ellipsize(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + c.width().unwrap_or(0) > width.saturating_sub(1) {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}
