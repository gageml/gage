//! Syntax highlighting via syntect, producing ratatui `Line`s ready to render.
//!
//! Syntect is used for tokenization only — its bundled themes are not loaded.
//! Per-scope styling is owned by `style_for_scope`, which routes to named ANSI
//! colors in `crate::style` so the user's terminal palette decides the hues.

use ratatui::text::{Line, Span};
use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::styles;

pub struct Highlighter {
    syntax_set: SyntaxSet,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
        }
    }

    pub fn highlight(&self, code: &str, language: &str) -> Vec<Line<'static>> {
        let code = code.replace('\t', "    ");
        let syntax = self
            .syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut state = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut lines = Vec::new();

        for line in LinesWithEndings::from(&code) {
            let ops = state.parse_line(line, &self.syntax_set).unwrap();
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (region, op) in ScopeRegionIterator::new(&ops, line) {
                let region = region.trim_end_matches('\n');
                if !region.is_empty() {
                    spans.push(Span::styled(region.to_string(), style_for_scope(&stack)));
                }
                stack.apply(op).unwrap();
            }
            lines.push(Line::from(spans));
        }

        lines
    }
}

/// Map the innermost scope on the stack to a style.
///
/// Note: syntect's YAML grammar gives bare scalar values no specific scope
/// (they sit at `source.yaml`), while keys land at `string.unquoted.plain.out`.
/// The `constant.*` arms are dormant for YAML and only fire in other languages.
fn style_for_scope(stack: &ScopeStack) -> ratatui::style::Style {
    for scope in stack.as_slice().iter().rev() {
        let s = scope.build_string();
        if s.starts_with("string.unquoted.plain.out") {
            return styles::Syntax::key();
        }
        if s.starts_with("comment") {
            return styles::Syntax::comment();
        }
        if s.starts_with("constant.language") {
            return styles::Syntax::constant();
        }
        if s.starts_with("constant.numeric") {
            return styles::Syntax::number();
        }
        if s.starts_with("string.quoted") {
            return styles::Syntax::string();
        }
    }
    ratatui::style::Style::new()
}
