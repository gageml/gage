use tabled::settings::{Color, peaker::Peaker};

pub use gage_core::style::{IdHighlighter, IdKind};

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
///
/// A column's weight scales its width for the comparison: a column of
/// weight 2 is picked only when it is more than twice as wide as a
/// column of weight 1, so at equilibrium it holds twice the width.
/// Columns without an explicit weight have weight 1.
pub struct IdAwarePriority {
    protect_id: bool,
    weights: Vec<usize>,
}

impl IdAwarePriority {
    pub fn new(protect_id: bool) -> Self {
        Self {
            protect_id,
            weights: Vec::new(),
        }
    }

    /// Set the weight of column `col`. A weight of zero is treated as 1.
    pub fn weight(mut self, col: usize, weight: usize) -> Self {
        if self.weights.len() <= col {
            self.weights.resize(col + 1, 1);
        }
        if let Some(slot) = self.weights.get_mut(col) {
            *slot = weight.max(1);
        }
        self
    }

    fn weight_of(&self, col: usize) -> usize {
        self.weights.get(col).copied().unwrap_or(1)
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
            // Compare w_a / weight_a against w_b / weight_b without division
            .max_by(|&(a, wa), &(b, wb)| (wa * self.weight_of(b)).cmp(&(wb * self.weight_of(a))))
            .map(|(i, _)| i)
    }
}

/// Styled id: the kind's bright shade over the unique prefix, its dark
/// shade for the rest of the shown form.
pub fn styled_id(shown: &str, prefix: &str, kind: IdKind) -> String {
    let split = shown
        .char_indices()
        .nth(prefix.chars().count())
        .map(|(i, _)| i)
        .unwrap_or(shown.len());
    let (head, tail) = shown.split_at(split);
    kind.style(head, tail)
}
