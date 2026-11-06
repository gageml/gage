//! Render an attachment entry into ratatui lines.

use ratatui::text::Line;

use crate::attrs::attr_lines;
use crate::doc::Attachment;

/// The attachment's type with its qualifier, then each rendered item
/// as the harness sent it, with a blank line before each.
pub fn body(attachment: &Attachment) -> Vec<Line<'static>> {
    let kind = match &attachment.qualifier {
        Some(qualifier) => format!("{} ({qualifier})", attachment.kind),
        None => attachment.kind.clone(),
    };
    let mut out = attr_lines(&[("Attachment type", &kind)]);
    for rendered in &attachment.rendered {
        out.push(Line::raw(""));
        out.extend(rendered.split('\n').map(|line| Line::raw(line.to_string())));
    }
    out
}
