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

/// Format a finished duration, keeping sub-second and tenths detail.
pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Format a still-ticking elapsed time at whole-second resolution, so
/// a live display advances once per second instead of every redraw.
pub(crate) fn fmt_duration_live(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}
