//! Render a message entry into ratatui lines.

use ratatui::text::Line;

use crate::attrs::attr_lines;
use crate::markdown;

/// The message text as markdown. A message without text is the tags
/// split off the front of it, as caption and value rows; a message
/// with text does not show its tags.
pub fn body(text: &str, tags: &[(String, String)], width: u16) -> Vec<Line<'static>> {
    if !text.trim().is_empty() {
        return markdown::render(text);
    }
    let attrs: Vec<(&str, &str)> = tags
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_str()))
        .collect();
    attr_lines(&attrs, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn tags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, c)| (n.to_string(), c.to_string()))
            .collect()
    }

    /// A message with text shows the text and not its tags.
    #[test]
    fn a_message_with_text_does_not_show_its_tags() {
        let lines = body("hello", &tags(&[("ide_opened_file", "a.rs")]), 80);
        assert_eq!(lines, markdown::render("hello"));
    }

    /// A message that is all tags shows them as rows; blank text
    /// counts as no text.
    #[test]
    fn a_message_of_tags_alone_is_its_rows() {
        let pairs = tags(&[("command-name", "/gage:publish"), ("command-args", "-y")]);
        assert_eq!(
            texts(&body("", &pairs, 80)),
            ["command-name  /gage:publish", "command-args  -y"]
        );
        assert_eq!(texts(&body(" \n", &pairs, 80)).len(), 2);
    }
}
