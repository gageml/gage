//! Render an entry's tool calls and tool results into ratatui lines.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::attrs::attr_lines;
use crate::doc::ToolUse;
use crate::styles;

/// The calls an entry made: each call's tool and its input, with a
/// blank line between calls.
pub fn calls(calls: &[ToolUse]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for call in calls {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.extend(attr_lines(&[("Tool", &call.name)]));
        out.push(Line::raw(""));
        append_maybe_json(&mut out, &call.input);
    }
    out
}

/// The results an entry returned: each result's tool, its error status
/// when it failed, and its output, with a blank line between results.
pub fn results(results: &[ToolUse]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for result in results {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.extend(attr_lines(&[("Tool", &result.name)]));
        if result.is_error == Some(true) {
            out.push(Line::from(vec![
                Span::styled("Status  ", styles::Text::dim()),
                Span::styled("error", Style::new().fg(Color::Red)),
            ]));
        }
        if let Some(output) = &result.output {
            out.push(Line::raw(""));
            append_maybe_json(&mut out, output);
        }
    }
    out
}

/// `text` pretty-printed when it parses as JSON, as it is otherwise.
/// Inputs are recorded compact and outputs are often structured, and
/// the indented form is easier to read.
fn append_maybe_json(out: &mut Vec<Line<'static>>, text: &str) {
    let trimmed = text.trim_start();
    if matches!(trimmed.as_bytes().first(), Some(b'{' | b'['))
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        append_plain(out, &pretty);
        return;
    }
    append_plain(out, text);
}

fn append_plain(out: &mut Vec<Line<'static>>, text: &str) {
    for line in text.split('\n') {
        out.push(Line::from(Span::raw(line.to_string())));
    }
}
