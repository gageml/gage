use std::io::{self, IsTerminal};

use gage_core::uuid::short_uuid;
use gage_store::Store;
use tabled::Table;
use tabled::settings::{Color, Width, peaker::Peaker};

pub use gage_core::style::{IdHighlighter, IdKind};

/// The id's short display for a dialog outro: the unique prefix in bold,
/// its color inherited from the green outro line. Peers come from the
/// same short-prefix set an id argument resolves against, so a shown
/// prefix resolves. On a peer-read error the id is shown plain and the
/// failure goes to stderr.
pub fn outro_id(store: &Store, object_type: &str, id: &str) -> String {
    match store.short_prefix_ids(Some(object_type)) {
        Ok(peers) => IdHighlighter::new(peers).bold_short(id),
        Err(e) => {
            eprintln!("warning: reading {object_type} ids: {e}");
            short_uuid(id).to_string()
        }
    }
}

/// The determinate progress bar template after its spinner, used by
/// the dialog theme for every bar.
pub const PROGRESS_TEMPLATE: &str =
    "{msg} [{elapsed_precise}] {bar:30.white/bright.black} ({pos}/{len})";

/// Line-style fill and remainder characters of the progress bar
pub const PROGRESS_CHARS: &str = "▬▬";

/// The indicator row's template once its bar shows: after the
/// spinner and label, a short dim bar and the count.
pub const INDICATOR_TEMPLATE: &str = "{msg} {bar:12.white.dim/bright.black} {pos}/{len}";

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

/// The width to fit output to: the terminal's columns, or `None` when
/// standard output is not a terminal and nothing is cut or wrapped, so
/// a piped listing carries every cell whole.
pub fn stdout_width() -> Option<usize> {
    io::stdout()
        .is_terminal()
        .then(|| console::Term::stdout().size().1 as usize)
}

/// Cut `table` to `width` columns, the widest column first under
/// `priority`, marking a cut cell with an ellipsis. Without a width
/// the table stays whole.
pub fn truncate_to(table: &mut Table, width: Option<usize>, priority: IdAwarePriority) {
    if let Some(width) = width {
        table.with(Width::truncate(width).suffix("…").priority(priority));
    }
}

/// The width of the value column of a two-column `label | value`
/// table at `width` columns with labels `label_width` wide: the rest
/// after the borders and padding, `"│ " + " │ " + " │"`, and never
/// under 20. Without a width the value is not wrapped.
pub fn value_width(width: Option<usize>, label_width: usize) -> usize {
    match width {
        Some(width) => width.saturating_sub(label_width + 8).max(20),
        None => usize::MAX,
    }
}

pub fn dim() -> Color {
    tty(Color::new("\x1b[2m", "\x1b[22m"))
}

pub fn dim_italic() -> Color {
    tty(Color::new("\x1b[2;3m", "\x1b[22;23m"))
}

/// Truncates the biggest column first (like `PriorityMax::left`), but never
/// picks the Id column (index 0 unless `id_column` names another) when
/// `protect_id` is set, so a full ID is preserved while the other
/// columns absorb the shrink.
///
/// A column's weight scales its width for the comparison: a column of
/// weight 2 is picked only when it is more than twice as wide as a
/// column of weight 1, so at equilibrium it holds twice the width.
/// Columns without an explicit weight have weight 1.
pub struct IdAwarePriority {
    protect_id: bool,
    id_col: usize,
    weights: Vec<usize>,
}

impl IdAwarePriority {
    pub fn new(protect_id: bool) -> Self {
        Self {
            protect_id,
            id_col: 0,
            weights: Vec::new(),
        }
    }

    /// Name the column `protect_id` preserves, when it is not the first.
    pub fn id_column(mut self, col: usize) -> Self {
        self.id_col = col;
        self
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
        widths
            .iter()
            .copied()
            .enumerate()
            .rev()
            .filter(|&(i, _)| !(self.protect_id && i == self.id_col))
            .filter(|&(i, w)| w != 0 && (mins.is_empty() || mins.get(i).is_none_or(|&m| w > m)))
            // Compare w_a / weight_a against w_b / weight_b without division
            .max_by(|&(a, wa), &(b, wb)| (wa * self.weight_of(b)).cmp(&(wb * self.weight_of(a))))
            .map(|(i, _)| i)
    }
}

/// Styled id: the kind's bright shade over the unique prefix, its dark
/// shade for the rest of the shown form.
pub fn styled_id(shown: &str, prefix: &str, kind: IdKind) -> String {
    styled_target(shown, prefix, kind, "")
}

/// A styled id followed by `suffix` in the id's dark shade: the form
/// of a `<session>:<line>` target, as `gage blame` prints one.
pub fn styled_target(shown: &str, prefix: &str, kind: IdKind, suffix: &str) -> String {
    let split = shown
        .char_indices()
        .nth(prefix.chars().count())
        .map(|(i, _)| i)
        .unwrap_or(shown.len());
    let (head, tail) = shown.split_at(split);
    kind.style(head, &format!("{tail}{suffix}"))
}

#[cfg(test)]
mod tests {
    use tabled::settings::Style;

    use super::*;

    /// A width cuts the widest column to fit and marks the cut; no
    /// width leaves every cell whole, which is what a piped listing
    /// needs.
    #[test]
    fn truncate_to_cuts_only_when_a_width_is_given() {
        let id = "0123456789abcdefghjkmnpqrs";
        let title = "a title long enough to be the widest column by far";
        let rows = vec![
            vec!["Id".to_string(), "Title".to_string()],
            vec![id.to_string(), title.to_string()],
        ];

        let mut cut = Table::from_iter(rows.clone());
        cut.with(Style::rounded());
        truncate_to(&mut cut, Some(50), IdAwarePriority::new(true));
        let cut = cut.to_string();
        assert!(cut.contains(id), "{cut}");
        assert!(cut.contains('…'), "{cut}");
        assert!(!cut.contains(title), "{cut}");
        assert!(cut.lines().all(|l| console::measure_text_width(l) <= 50));

        let mut whole = Table::from_iter(rows);
        whole.with(Style::rounded());
        truncate_to(&mut whole, None, IdAwarePriority::new(true));
        let whole = whole.to_string();
        assert!(whole.contains(id), "{whole}");
        assert!(whole.contains(title), "{whole}");
        assert!(!whole.contains('…'), "{whole}");
    }

    #[test]
    fn value_width_leaves_room_for_the_label_and_never_wraps_without_a_width() {
        assert_eq!(value_width(Some(80), 5), 67);
        assert_eq!(value_width(Some(30), 5), 20);
        assert_eq!(value_width(None, 5), usize::MAX);
    }
}
