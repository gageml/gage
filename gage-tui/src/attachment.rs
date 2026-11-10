//! Render an attachment entry into ratatui lines.

use ratatui::text::Line;

use crate::attrs::attr_lines;
use crate::doc::Attachment;

/// The attachment's type, its subtype when it has one, and its
/// selected attributes, then each text item with a blank line before
/// it.
pub fn body(attachment: &Attachment, width: u16) -> Vec<Line<'static>> {
    let mut attrs = vec![("Type", attachment.kind.as_str())];
    if let Some(subtype) = &attachment.subtype {
        attrs.push(("Subtype", subtype.as_str()));
    }
    attrs.extend(
        attachment
            .attrs
            .iter()
            .map(|(c, v)| (c.as_str(), v.as_str())),
    );
    let mut out = attr_lines(&attrs, width);
    for rendered in &attachment.rendered {
        out.push(Line::raw(""));
        out.extend(rendered.split('\n').map(|line| Line::raw(line.to_string())));
    }
    out
}
