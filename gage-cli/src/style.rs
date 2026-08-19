use tabled::settings::{Color, peaker::Peaker};

pub use gage_core::style::IdHighlighter;

pub fn spinner(message: &str) -> indicatif::ProgressBar {
    let spinner = indicatif::ProgressBar::new_spinner();
    spinner
        .set_style(indicatif::ProgressStyle::with_template("{spinner:.magenta}  {msg}").unwrap());
    spinner.set_message(message.to_string());
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner
}

/// Returns `c` when colors are enabled for stdout, else an empty `Color`.
/// Wrap every `tabled::settings::Color` used in listing tables so table
/// styling honors the same TTY gate as `console::style`, avoiding the
/// half-colored output that mixes bare ID columns with escape-wrapped
/// headers when the CLI is piped.
pub fn tty(c: Color) -> Color {
    if console::colors_enabled() {
        c
    } else {
        Color::new("", "")
    }
}

pub fn dim() -> Color {
    tty(Color::new("\x1b[2m", "\x1b[22m"))
}

pub fn dim_italic() -> Color {
    tty(Color::new("\x1b[2;3m", "\x1b[22;23m"))
}

/// Truncates the biggest column first (like `PriorityMax::left`), but never
/// picks the Id column (index 0) when `protect_id` is set, so a full ID is
/// preserved while the other columns absorb the shrink.
pub struct IdAwarePriority {
    protect_id: bool,
}

impl IdAwarePriority {
    pub fn new(protect_id: bool) -> Self {
        Self { protect_id }
    }
}

impl Peaker for IdAwarePriority {
    fn peak(&mut self, mins: &[usize], widths: &[usize]) -> Option<usize> {
        let start = if self.protect_id { 1 } else { 0 };
        widths
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .rev()
            .filter(|&(i, w)| w != 0 && (mins.is_empty() || mins.get(i).is_none_or(|&m| w > m)))
            .max_by_key(|&(_, w)| w)
            .map(|(i, _)| i)
    }
}
