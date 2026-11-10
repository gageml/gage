//! Render a message entry into ratatui lines.

use ratatui::text::Line;

use crate::attrs::attr_lines;
use crate::markdown;

/// The message text as markdown, then the tags split off the front of
/// it as caption and value rows after a blank line. A message without
/// text is its tag rows alone.
pub fn body(text: &str, tags: &[(String, String)]) -> Vec<Line<'static>> {
    let mut out = if text.trim().is_empty() {
        Vec::new()
    } else {
        markdown::render(text)
    };
    if tags.is_empty() {
        return out;
    }
    if !out.is_empty() {
        out.push(Line::raw(""));
    }
    let attrs: Vec<(&str, &str)> = tags
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_str()))
        .collect();
    out.extend(attr_lines(&attrs));
    out
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

    /// The tag rows follow the message text after one blank line.
    #[test]
    fn tag_rows_follow_the_text() {
        let lines = body("hello", &tags(&[("ide_opened_file", "a.rs")]));
        let texts = texts(&lines);
        assert_eq!(texts.last().unwrap(), "ide_opened_file  a.rs");
        assert_eq!(texts[texts.len() - 2], "");
        assert!(texts[0].contains("hello"), "{texts:?}");
    }

    /// A message that is all tags shows the rows with no blank line
    /// above them; a message without tags is its text alone.
    #[test]
    fn a_message_of_tags_alone_is_its_rows() {
        let pairs = tags(&[("command-name", "/gage:publish"), ("command-args", "-y")]);
        assert_eq!(
            texts(&body("", &pairs)),
            ["command-name  /gage:publish", "command-args  -y"]
        );
        assert_eq!(body("hello", &[]), markdown::render("hello"));
    }
}
