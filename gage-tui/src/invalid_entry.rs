//! Render an invalid entry, a native line the codec could not decode,
//! into ratatui lines.

use ratatui::text::Line;

use crate::attrs::attr_lines;
use crate::doc::InvalidEntry;

/// The codec's error as an attribute, then the raw line as plain text
/// after a blank line.
pub fn body(invalid: &InvalidEntry) -> Vec<Line<'static>> {
    let mut out = attr_lines(&[("Error", invalid.error.as_str())]);
    out.push(Line::raw(""));
    out.extend(
        invalid
            .raw
            .split('\n')
            .map(|line| Line::raw(line.to_string())),
    );
    out
}
