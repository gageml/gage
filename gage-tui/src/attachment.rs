//! Render an attachment entry into ratatui lines.

use ratatui::text::Line;

use crate::attrs::attr_lines;
use crate::doc::Attachment;

/// The attachment's type and its subtype when it has one, then each
/// rendered item as the harness sent it, with a blank line before
/// each.
pub fn body(attachment: &Attachment) -> Vec<Line<'static>> {
    let mut attrs = vec![("Type", attachment.kind.as_str())];
    if let Some(subtype) = &attachment.subtype {
        attrs.push(("Subtype", subtype.as_str()));
    }
    let mut out = attr_lines(&attrs);
    for rendered in &attachment.rendered {
        out.push(Line::raw(""));
        out.extend(rendered.split('\n').map(|line| Line::raw(line.to_string())));
    }
    out
}
