//! Semantic styles for the TUI.
//!
//! Every styling decision in the app routes through this module so adjusting
//! the look is a one-file change. Built from ANSI modifiers and named colors
//! only — both honor the user's terminal palette. If a specific shade is ever
//! required, add it here with a comment explaining why.

use ratatui::style::{Color, Modifier, Style};

pub fn header() -> Style {
    Style::new().fg(Color::White).bg(Color::Black)
}

pub fn footer() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn panel_border(active: bool) -> Style {
    if active {
        Style::new()
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

pub fn selection() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

pub fn scrollbar(active: bool) -> Style {
    if active {
        Style::new()
    } else {
        Style::new().add_modifier(Modifier::DIM)
    }
}

pub fn text_dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn syntax_key() -> Style {
    Style::new().fg(Color::Cyan)
}

pub fn syntax_string() -> Style {
    Style::new().fg(Color::Green)
}

pub fn syntax_number() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn syntax_const() -> Style {
    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
}

pub fn syntax_comment() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn md_heading() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

pub fn md_code() -> Style {
    Style::new().fg(Color::LightCyan)
}

pub fn md_link() -> Style {
    Style::new()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::UNDERLINED)
}

pub fn md_blockquote() -> Style {
    Style::new().fg(Color::Yellow)
}
