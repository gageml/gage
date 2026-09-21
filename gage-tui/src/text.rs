//! Text display helpers shared across views.

use std::time::Duration;

use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Break styled lines at `width` cells so every returned line fits
/// without wrapping. Breaks fall at cell boundaries, not words; a span
/// that straddles a break is split into two spans with the same
/// style. A wide character that would cross the boundary moves to the
/// next line. One linear pass over the text, for content whose
/// per-render word wrapping would be too slow.
pub(crate) fn hard_wrap(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for span in line.spans {
            let style = span.style;
            let mut piece = String::new();
            for c in span.content.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width && used > 0 {
                    if !piece.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut piece), style));
                    }
                    out.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                }
                piece.push(c);
                used += w;
            }
            if !piece.is_empty() {
                current.push(Span::styled(piece, style));
            }
        }
        out.push(Line::from(current));
    }
    out
}

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
    if ms == 0 {
        "<1ms".to_string()
    } else if ms < 1000 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn hard_wrap_breaks_at_cells_and_keeps_span_styles() {
        let red = Style::new().fg(Color::Red);
        let line = Line::from(vec![Span::raw("abcde"), Span::styled("fghij", red)]);
        let wrapped = hard_wrap(vec![line], 4);
        assert_eq!(texts(&wrapped), ["abcd", "efgh", "ij"]);
        let second = &wrapped[1];
        assert_eq!(second.spans.len(), 2);
        assert_eq!(second.spans[0].content, "e");
        assert_eq!(second.spans[1].content, "fgh");
        assert_eq!(second.spans[1].style, red);
    }

    #[test]
    fn hard_wrap_moves_a_wide_char_that_would_cross_the_edge() {
        let wrapped = hard_wrap(vec![Line::raw("ab日本")], 3);
        assert_eq!(texts(&wrapped), ["ab", "日", "本"]);
    }

    #[test]
    fn hard_wrap_keeps_short_and_empty_lines() {
        let wrapped = hard_wrap(vec![Line::raw("ab"), Line::raw("")], 10);
        assert_eq!(texts(&wrapped), ["ab", ""]);
    }
}
