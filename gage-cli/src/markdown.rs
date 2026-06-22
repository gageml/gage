//! Render a markdown string to ANSI-styled, width-wrapped text for
//! display inside a `tabled` cell. Subset of CommonMark covering what
//! note docstrings actually use: headings, emphasis, inline code, code
//! blocks, bullet/ordered lists, links, blockquotes, paragraphs.

use console::Style;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// Render `input` as ANSI-styled text wrapped to `width` columns.
/// Visible-width tracking ignores SGR escape sequences so wrapping
/// stays accurate when styles are present.
pub fn render(input: &str, width: usize) -> String {
    let parser = Parser::new_ext(
        input,
        Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH,
    );
    let mut r = Renderer::new(width.max(8));
    for event in parser {
        r.handle(event);
    }
    r.finish()
}

#[derive(Clone, Copy, Default)]
struct Inline {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

impl Inline {
    fn apply(self, text: &str) -> String {
        if !(self.bold || self.italic || self.strike || self.code || self.link) {
            return text.to_string();
        }
        let mut s = Style::new().force_styling(true);
        if self.bold {
            s = s.bold();
        }
        if self.italic {
            s = s.italic();
        }
        if self.strike {
            s = s.strikethrough();
        }
        if self.link {
            s = s.underlined();
        }
        if self.code {
            s = s.yellow();
        }
        s.apply_to(text).to_string()
    }
}

#[derive(Clone, Copy)]
enum BlockKind {
    Paragraph,
    Heading,
    CodeBlock,
    BlockQuote,
    Item { ordered: bool, index: u64 },
}

struct Token {
    /// True when this token should be preceded by a single space when
    /// rendered on the same line as the previous token. False at the
    /// start of a block, and after tokens whose immediate predecessor
    /// in the source had no intervening whitespace (e.g. punctuation
    /// after a code span).
    sep: bool,
    visible: String,
    styled: String,
}

struct Block {
    kind: BlockKind,
    tokens: Vec<Token>,
    /// Continuation indent applied to wrapped lines.
    indent: String,
    /// First-line marker rendered before the first token (e.g. `- `,
    /// `1. `).
    marker: String,
}

struct Renderer {
    width: usize,
    out: String,
    blocks: Vec<Block>,
    inline: Inline,
    pending_text: String,
    /// Whether the next flushed token should have `sep: true`. Set by
    /// whitespace in `push_text`, cleared by `flush_word`. Resets to
    /// false at the start of each new block.
    sep_pending: bool,
    list_stack: Vec<ListInfo>,
    blockquote_depth: usize,
}

#[derive(Clone, Copy)]
struct ListInfo {
    ordered: bool,
    next_index: u64,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: String::new(),
            blocks: Vec::new(),
            inline: Inline::default(),
            pending_text: String::new(),
            sep_pending: false,
            list_stack: Vec::new(),
            blockquote_depth: 0,
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(t) => {
                self.flush_word();
                let saved = self.inline;
                self.inline.code = true;
                self.push_text(&t);
                self.flush_word();
                self.inline = saved;
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => {
                self.flush_word();
                self.emit_block_and_pop();
                self.push_block(BlockKind::Paragraph);
            }
            Event::Rule => {
                self.emit_blank_line();
                self.out.push_str(&"─".repeat(self.width.min(40)));
                self.out.push('\n');
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.push_block(BlockKind::Paragraph),
            Tag::Heading { .. } => {
                self.push_block(BlockKind::Heading);
                self.inline.bold = true;
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth += 1;
                self.push_block(BlockKind::BlockQuote);
            }
            Tag::CodeBlock(_) => self.push_block(BlockKind::CodeBlock),
            Tag::List(start) => self.list_stack.push(ListInfo {
                ordered: start.is_some(),
                next_index: start.unwrap_or(1),
            }),
            Tag::Item => {
                let info = self.list_stack.last_mut().expect("list item outside list");
                let kind = BlockKind::Item {
                    ordered: info.ordered,
                    index: info.next_index,
                };
                info.next_index += 1;
                self.push_block(kind);
            }
            Tag::Emphasis => {
                self.flush_word();
                self.inline.italic = true;
            }
            Tag::Strong => {
                self.flush_word();
                self.inline.bold = true;
            }
            Tag::Strikethrough => {
                self.flush_word();
                self.inline.strike = true;
            }
            Tag::Link { .. } => {
                self.flush_word();
                self.inline.link = true;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::Item => self.emit_block_and_pop(),
            TagEnd::Heading(_) => {
                self.inline.bold = false;
                self.emit_block_and_pop();
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth -= 1;
                self.emit_block_and_pop();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::Emphasis => {
                self.flush_word();
                self.inline.italic = false;
            }
            TagEnd::Strong => {
                self.flush_word();
                self.inline.bold = false;
            }
            TagEnd::Strikethrough => {
                self.flush_word();
                self.inline.strike = false;
            }
            TagEnd::Link => {
                self.flush_word();
                self.inline.link = false;
            }
            _ => {}
        }
    }

    fn push_block(&mut self, kind: BlockKind) {
        let (marker, indent) = self.prefixes(kind);
        self.sep_pending = false;
        self.blocks.push(Block {
            kind,
            tokens: Vec::new(),
            indent,
            marker,
        });
    }

    fn prefixes(&self, kind: BlockKind) -> (String, String) {
        let bq = "│ ".repeat(self.blockquote_depth);
        let list_indent: String = self
            .list_stack
            .iter()
            .take(self.list_stack.len().saturating_sub(1))
            .map(|_| "  ")
            .collect();
        let base = format!("{bq}{list_indent}");
        match kind {
            BlockKind::Item { ordered, index } => {
                let marker = if ordered {
                    format!("{base}{index}. ")
                } else {
                    format!("{base}- ")
                };
                let indent = " ".repeat(marker.chars().count());
                (marker, indent)
            }
            BlockKind::CodeBlock => (format!("{base}    "), format!("{base}    ")),
            BlockKind::BlockQuote => (base.clone(), base),
            _ => (base.clone(), base),
        }
    }

    fn push_text(&mut self, text: &str) {
        // Code blocks preserve newlines literally; everything else
        // collapses whitespace into word boundaries.
        if matches!(
            self.blocks.last().map(|b| b.kind),
            Some(BlockKind::CodeBlock)
        ) {
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.flush_word();
                    self.emit_block_and_pop();
                    self.push_block(BlockKind::CodeBlock);
                }
                self.pending_text.push_str(line);
            }
            return;
        }
        for ch in text.chars() {
            if ch.is_whitespace() {
                self.flush_word();
                self.sep_pending = true;
            } else {
                self.pending_text.push(ch);
            }
        }
    }

    fn flush_word(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let visible = std::mem::take(&mut self.pending_text);
        let styled = self.inline.apply(&visible);
        let sep = self.sep_pending;
        self.sep_pending = false;
        if let Some(block) = self.blocks.last_mut() {
            block.tokens.push(Token {
                sep,
                visible,
                styled,
            });
        }
    }

    fn emit_block_and_pop(&mut self) {
        self.flush_word();
        let Some(block) = self.blocks.pop() else {
            return;
        };
        self.emit_block(&block);
    }

    fn emit_block(&mut self, block: &Block) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            if !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            // Headings and items skip the leading blank line when
            // following an item in the same list; paragraphs always
            // separate.
            if !matches!(block.kind, BlockKind::Item { .. }) {
                self.out.push('\n');
            }
        }

        if block.tokens.is_empty() {
            return;
        }

        let first_indent = block.marker.clone();
        let cont_indent = block.indent.clone();
        let mut line_visible = first_indent.chars().count();
        let max = self.width;
        let mut current = String::new();
        current.push_str(&first_indent);
        let mut at_line_start = true;

        for token in &block.tokens {
            let vw = token.visible.chars().count();
            let space = if at_line_start || !token.sep { 0 } else { 1 };
            if !at_line_start && line_visible + space + vw > max {
                current.push('\n');
                self.out.push_str(&current);
                current.clear();
                current.push_str(&cont_indent);
                line_visible = cont_indent.chars().count();
                at_line_start = true;
            }
            if !at_line_start && token.sep {
                current.push(' ');
                line_visible += 1;
            }
            current.push_str(&token.styled);
            line_visible += vw;
            at_line_start = false;
        }
        current.push('\n');
        self.out.push_str(&current);
    }

    fn emit_blank_line(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            if !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            self.out.push('\n');
        }
    }

    fn finish(mut self) -> String {
        while !self.blocks.is_empty() {
            self.emit_block_and_pop();
        }
        while self.out.ends_with('\n') {
            self.out.pop();
        }
        self.out
    }
}
