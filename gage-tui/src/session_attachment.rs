//! Render a session attachment and its files into ratatui lines.

use gage_core::datetime::format_elapsed_ms;
use gage_core::human::format_size;
use gage_core::path::shorten_home;
use ratatui::text::{Line, Span};

use crate::attrs::attr_lines;
use crate::doc::{AttachmentFile, SessionAttachment};
use crate::styles;

/// The attachment's attributes as caption/value rows, an absent name,
/// key, or target list omitted, then its files under a `Files`
/// heading when it has any.
pub fn body(attachment: &SessionAttachment) -> Vec<Line<'static>> {
    let targets = attachment.targets.join("\n");
    let root = shorten_home(&attachment.root);
    let file_count = attachment.file_count.to_string();
    let size = format_size(attachment.size);
    let created = format_elapsed_ms(attachment.created_ms);
    let modified = format_elapsed_ms(attachment.modified_ms);
    let mut attrs = vec![("Id", attachment.id.as_str())];
    if let Some(name) = &attachment.name {
        attrs.push(("Name", name));
    }
    if let Some(key) = &attachment.key {
        attrs.push(("Key", key));
    }
    if !targets.is_empty() {
        attrs.push(("Targets", &targets));
    }
    attrs.extend([
        ("Root", root.as_str()),
        ("Files", &file_count),
        ("Size", &size),
        ("Created", &created),
        ("Modified", &modified),
    ]);
    let mut out = attr_lines(&attrs);
    if attachment.files.is_empty() {
        return out;
    }
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled("Files", styles::Text::dim())));
    out.push(Line::raw(""));
    out.extend(file_lines(&attachment.files));
    out
}

/// One line per file: the path padded to the widest path, then the
/// size, so the sizes share a column.
fn file_lines(files: &[AttachmentFile]) -> Vec<Line<'static>> {
    let width = files
        .iter()
        .map(|f| f.path.chars().count())
        .max()
        .unwrap_or(0);
    files
        .iter()
        .map(|f| {
            Line::from(vec![
                Span::raw(format!("{:<width$}  ", f.path)),
                Span::styled(format_size(f.size), styles::Text::dim()),
            ])
        })
        .collect()
}

/// The file's text line by line, or a placeholder for a file whose
/// bytes are not UTF-8.
pub fn file_body(file: &AttachmentFile) -> Vec<Line<'static>> {
    match &file.text {
        Some(text) => text.lines().map(|l| Line::raw(l.to_string())).collect(),
        None => vec![Line::from(Span::styled(
            "(binary file)",
            styles::Text::dim(),
        ))],
    }
}
