//! Render a Claude `message` value into ratatui lines.
//!
//! A message's `content` is either a string or a sequence of typed blocks
//! (`text`, `thinking`, `tool_use`, `tool_result`, ...). Each block is turned
//! into a labeled section; prose `text` blocks are routed through the markdown
//! renderer.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::attrs::attr_lines;
use crate::markdown;
use crate::styles;

pub fn render(message: &Value) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let Some(content) = message.get("content") else {
        return out;
    };
    match content {
        Value::String(text) => append_plain(&mut out, text, Style::new()),
        Value::Array(blocks) => {
            for block in blocks {
                if !out.is_empty() {
                    out.push(Line::raw(""));
                }
                append_block(&mut out, block);
            }
        }
        _ => {}
    }
    out
}

fn append_block(out: &mut Vec<Line<'static>>, block: &Value) {
    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
    match block_type {
        "text" => {
            if let Some(text) = block.get("text").and_then(Value::as_str)
                && !is_client_tag(text)
            {
                out.extend(markdown::render(text));
            }
        }
        "thinking" => {
            let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
            if !text.is_empty() {
                append_plain(out, text, styles::Text::dim());
            }
        }
        "tool_use" => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
            out.extend(attr_lines(&[("Tool", name)]));
            if let Some(input) = block.get("input") {
                out.push(Line::raw(""));
                let pretty =
                    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
                append_plain(out, &pretty, Style::new());
            }
        }
        "tool_reference" => {
            let name = block.get("tool_name").and_then(Value::as_str).unwrap_or("");
            out.extend(attr_lines(&[("Tool", name)]));
        }
        "tool_result" => {
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                out.push(Line::from(vec![
                    Span::styled("Status  ", styles::Text::dim()),
                    Span::styled("error", Style::new().fg(Color::Red)),
                ]));
                out.push(Line::raw(""));
            }
            match block.get("content") {
                Some(Value::String(text)) => append_maybe_json(out, text, Style::new()),
                Some(Value::Array(blocks)) => {
                    for inner in blocks {
                        append_tool_result_block(out, inner);
                    }
                }
                _ => {}
            }
        }
        other => {
            if !other.is_empty() {
                out.extend(attr_lines(&[("Type", other)]));
            }
        }
    }
}

/// Inner blocks of a `tool_result` — text is plain (not markdown) so
/// tool output like YAML or JSON isn't reinterpreted as headings/lists.
fn append_tool_result_block(out: &mut Vec<Line<'static>>, block: &Value) {
    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
    if block_type == "text" {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            append_maybe_json(out, text, Style::new());
        }
    } else {
        append_block(out, block);
    }
}

/// Returns true when `text`, after trimming, is fully enclosed in a matching
/// `<tag>…</tag>` pair — i.e. synthetic content injected by the client
/// (`<ide_opened_file>`, `<system-reminder>`, etc.) rather than user prose.
fn is_client_tag(text: &str) -> bool {
    let trimmed = text.trim();
    let rest = match trimmed.strip_prefix('<') {
        Some(r) => r,
        None => return false,
    };
    let open_end = match rest.find('>') {
        Some(i) => i,
        None => return false,
    };
    let header = &rest[..open_end];
    let name = header
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
    {
        return false;
    }
    trimmed.ends_with(&format!("</{name}>"))
}

/// If `text` parses as JSON, render it pretty-printed; otherwise render
/// it as-is. Used for `tool_result` content where structured payloads are
/// common and the indented form is easier to read.
fn append_maybe_json(out: &mut Vec<Line<'static>>, text: &str, style: Style) {
    let trimmed = text.trim_start();
    if matches!(trimmed.as_bytes().first(), Some(b'{' | b'['))
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        append_plain(out, &pretty, style);
        return;
    }
    append_plain(out, text, style);
}

fn append_plain(out: &mut Vec<Line<'static>>, text: &str, style: Style) {
    for line in text.split('\n') {
        out.push(Line::from(Span::styled(line.to_string(), style)));
    }
}
