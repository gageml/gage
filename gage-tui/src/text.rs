//! Text display helpers shared across views.

use std::time::Duration;

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

pub(crate) fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        match d.as_millis() {
            0 => "<1ms".to_string(),
            ms => format!("{ms}ms"),
        }
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}
