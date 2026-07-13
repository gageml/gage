//! Markdown → ratatui Lines renderer.
//!
//! Walks `pulldown-cmark` events and emits styled `Vec<Line<'static>>` for the
//! body pane. Width-aware wrapping is applied later by ratatui's `Paragraph`
//! widget with `Wrap { trim: false }`, which preserves the leading indent and
//! bullet prefix we bake into each line here.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::styles;

pub fn render(input: &str) -> Vec<Line<'static>> {
    let parser = Parser::new_ext(
        input,
        Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH,
    );
    let mut r = Renderer::default();
    for event in parser {
        r.handle(event);
    }
    r.finish()
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListInfo>,
    in_code_block: bool,
    blockquote_depth: usize,
    item_prefix: Option<String>,
    line_indent: usize,
}

struct ListInfo {
    ordered: bool,
    index: u64,
}

impl Renderer {
    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.push_span(Span::styled(t.to_string(), styles::Markdown::code())),
            Event::SoftBreak => self.push_span(Span::raw(" ")),
            Event::HardBreak => self.flush_line(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                self.push_span(Span::raw(mark.to_string()));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block_break(),
            Tag::Heading { level, .. } => {
                self.block_break();
                let hashes = "#".repeat(heading_level(level));
                self.push_span(Span::styled(
                    format!("{hashes} "),
                    styles::Markdown::heading(),
                ));
                self.style_stack.push(styles::Markdown::heading());
            }
            Tag::BlockQuote(_) => {
                self.block_break();
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(_) => {
                self.block_break();
                self.in_code_block = true;
            }
            Tag::List(start) => {
                self.block_break();
                self.list_stack.push(ListInfo {
                    ordered: start.is_some(),
                    index: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let prefix = if let Some(list) = self.list_stack.last_mut() {
                    if list.ordered {
                        let n = list.index;
                        list.index += 1;
                        format!("{indent}{n}. ")
                    } else {
                        format!("{indent}- ")
                    }
                } else {
                    indent.clone()
                };
                self.line_indent = prefix.chars().count();
                self.item_prefix = Some(prefix);
            }
            Tag::Emphasis => self
                .style_stack
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .style_stack
                .push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .style_stack
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self.style_stack.push(styles::Markdown::link()),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.flush_line();
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.line_indent = 0;
                }
            }
            TagEnd::Item => {
                self.flush_line();
                self.item_prefix = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.style_stack.pop();
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        let style = self.cur_style();
        if self.in_code_block {
            for (i, line) in t.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                }
                if !line.is_empty() {
                    self.push_span(Span::styled(line.to_string(), styles::Markdown::code()));
                }
            }
        } else {
            self.push_span(Span::styled(t.to_string(), style));
        }
    }

    fn cur_style(&self) -> Style {
        let mut s = Style::new();
        for layer in &self.style_stack {
            s = s.patch(*layer);
        }
        s
    }

    fn push_span(&mut self, span: Span<'static>) {
        if self.cur.is_empty() {
            self.apply_line_start();
        }
        self.cur.push(span);
    }

    fn apply_line_start(&mut self) {
        if let Some(prefix) = self.item_prefix.take() {
            self.cur.push(Span::raw(prefix));
            return;
        }
        if self.line_indent > 0 {
            self.cur.push(Span::raw(" ".repeat(self.line_indent)));
        }
        if self.blockquote_depth > 0 {
            self.cur.push(Span::styled(
                "│ ".repeat(self.blockquote_depth),
                styles::Markdown::blockquote(),
            ));
        }
    }

    fn block_break(&mut self) {
        let has_content =
            !self.cur.is_empty() || self.lines.last().is_some_and(|l| !l.spans.is_empty());
        self.flush_line();
        if has_content
            && self.list_stack.is_empty()
            && self.blockquote_depth == 0
            && self.lines.last().is_some_and(|l| !l.spans.is_empty())
        {
            self.lines.push(Line::default());
        }
    }

    fn flush_line(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.cur);
        self.lines.push(Line::from(spans));
    }

    fn rule(&mut self) {
        self.block_break();
        self.push_span(Span::styled("─".repeat(20), styles::Text::dim()));
        self.flush_line();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
