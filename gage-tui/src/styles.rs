//! Style schemes for the TUI, one struct per concern.
//!
//! Every styling decision routes through this module so adjusting the
//! look is a one-file change. Built from ANSI modifiers and named
//! colors only — both honor the user's terminal palette. If a specific
//! shade is ever required, add it here with a comment explaining why.
//!
//! Each struct owns a scheme; the same color appearing in two schemes
//! is deliberate duplication, not sharing — a scheme can change its
//! colors without auditing anyone else's call sites.

use ratatui::style::{Color, Modifier, Style};

/// Structural chrome: panel borders, scrollbars, selections, the
/// app-level header/footer bars, and the progress gauge.
pub(crate) struct Panel;

impl Panel {
    /// Focused panels carry a bright-yellow border; unfocused dim.
    pub fn border(active: bool) -> Style {
        if active {
            Style::new()
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    pub fn scrollbar(active: bool) -> Style {
        if active {
            Style::new()
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    /// Selected row: reversed in the focused panel, muted gray
    /// elsewhere so it does not read as the active selection.
    pub fn selection(active: bool) -> Style {
        if active {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(Color::DarkGray)
        }
    }

    pub fn header() -> Style {
        Style::new().fg(Color::White).bg(Color::Black)
    }

    pub fn footer() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    pub fn gauge() -> Style {
        Style::new().fg(Color::Cyan)
    }
}

/// Message dialog surface — reverse video so it stands out against
/// the view beneath it.
pub(crate) struct Dialog;

impl Dialog {
    pub fn surface() -> Style {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

/// Tracing log levels, as rendered in the scan log dialog. The `.err`
/// stream and PANIC lines use [`LogLevel::error`].
pub(crate) struct LogLevel;

impl LogLevel {
    pub fn error() -> Style {
        Style::new().fg(Color::Red)
    }

    pub fn warn() -> Style {
        Style::new().fg(Color::Yellow)
    }

    pub fn info() -> Style {
        Style::new().fg(Color::Green)
    }

    pub fn debug() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// Scan task states, as rendered in the tasks table. The Errors badge
/// counts failed tasks and uses [`RunStatus::error`].
pub(crate) struct RunStatus;

impl RunStatus {
    pub fn pending() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    pub fn running() -> Style {
        Style::new().fg(Color::Yellow)
    }

    pub fn completed() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    pub fn skipped() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    pub fn error() -> Style {
        Style::new().fg(Color::Red)
    }
}

/// Content text roles.
pub(crate) struct Text;

impl Text {
    /// Secondary text: captions, table headers, empty-state messages
    pub fn dim() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    /// Emphasized quoted content, e.g. evidence note values in the
    /// issue detail
    pub fn accent() -> Style {
        Style::new().fg(Color::LightCyan)
    }

    /// Full-width section header, e.g. message sections in the session
    /// dialog. DarkGray is the one palette slot themes map to a
    /// with-the-theme gray; no relative lighten/darken exists in ANSI.
    pub fn header() -> Style {
        Style::new().bg(Color::DarkGray)
    }

    /// Secondary text within a section header (e.g. the line number)
    pub fn header_dim() -> Style {
        Style::new().bg(Color::DarkGray).add_modifier(Modifier::DIM)
    }
}

/// JSON syntax highlighting (see `syntax.rs`).
pub(crate) struct Syntax;

impl Syntax {
    pub fn key() -> Style {
        Style::new().fg(Color::Cyan)
    }

    pub fn string() -> Style {
        Style::new().fg(Color::Green)
    }

    pub fn number() -> Style {
        Style::new().fg(Color::Yellow)
    }

    pub fn constant() -> Style {
        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
    }

    pub fn comment() -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// Markdown rendering (see `markdown.rs`).
pub(crate) struct Markdown;

impl Markdown {
    pub fn heading() -> Style {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }

    pub fn code() -> Style {
        Style::new().fg(Color::LightCyan)
    }

    pub fn link() -> Style {
        Style::new()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::UNDERLINED)
    }

    pub fn blockquote() -> Style {
        Style::new().fg(Color::Yellow)
    }
}
