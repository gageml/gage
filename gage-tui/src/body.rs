//! Render the body a driver describes for an entry into ratatui lines.

use gage_session::{BodyBlock, EntryBody};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::attrs::attr_lines;
use crate::{markdown, styles};

/// The body's blocks top to bottom, fitted to `width` columns, with a
/// blank line before each block but the first. A block directly under
/// a heading follows it without one.
pub fn lines(body: &EntryBody, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut under_heading = false;
    for block in &body.blocks {
        if !out.is_empty() && !under_heading {
            out.push(Line::raw(""));
        }
        under_heading = matches!(block, BodyBlock::Heading(_));
        out.extend(block_lines(block, width));
    }
    out
}

fn block_lines(block: &BodyBlock, width: u16) -> Vec<Line<'static>> {
    match block {
        BodyBlock::Attrs(attrs) => {
            let attrs: Vec<(&str, &str)> = attrs
                .iter()
                .map(|a| (a.caption.as_str(), a.value.as_str()))
                .collect();
            attr_lines(&attrs, width)
        }
        BodyBlock::Heading(text) => vec![styled(text, Style::new().add_modifier(Modifier::BOLD))],
        BodyBlock::Markdown(text) => markdown::render(text),
        BodyBlock::Text(text) => text
            .split('\n')
            .map(|line| Line::raw(line.to_string()))
            .collect(),
        BodyBlock::Remark(text) => {
            vec![styled(
                text,
                styles::Text::dim().add_modifier(Modifier::ITALIC),
            )]
        }
    }
}

fn styled(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), style))
}

#[cfg(test)]
mod tests {
    use gage_session::BodyAttr;

    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// Blocks are separated by one blank line, a heading sits directly
    /// over the block after it, and a remark is dim italic.
    #[test]
    fn blocks_render_in_order_with_blank_lines_between() {
        let body = EntryBody {
            blocks: vec![
                BodyBlock::Attrs(vec![BodyAttr::new("totalCostUSD", "0.5")]),
                BodyBlock::Heading("claude-x".to_string()),
                BodyBlock::Attrs(vec![BodyAttr::new("inputTokens", "34")]),
                BodyBlock::Text("first\nsecond".to_string()),
                BodyBlock::Remark("Empty".to_string()),
            ],
        };
        let lines = lines(&body, 80);
        assert_eq!(
            texts(&lines),
            [
                "totalCostUSD  0.5",
                "",
                "claude-x",
                "inputTokens  34",
                "",
                "first",
                "second",
                "",
                "Empty",
            ]
        );
        let heading = lines[2].spans[0].style;
        assert!(heading.add_modifier.contains(Modifier::BOLD));
        let remark = lines[8].spans[0].style;
        assert!(
            remark
                .add_modifier
                .contains(Modifier::ITALIC | Modifier::DIM)
        );
    }

    /// A markdown block goes through the markdown renderer.
    #[test]
    fn a_markdown_block_is_rendered_as_markdown() {
        let body = EntryBody {
            blocks: vec![BodyBlock::Markdown("Hello **world**".to_string())],
        };
        assert_eq!(lines(&body, 80), markdown::render("Hello **world**"));
    }
}
