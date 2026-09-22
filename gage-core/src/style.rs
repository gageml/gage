//! Terminal display helpers shared by the CLI binaries.

use console::style;

use crate::uuid::short_uuid;

/// The namespace an ID belongs to, which fixes its terminal color.
///
/// Gage object IDs (sessions in the store, notes, issues, scans) are
/// yellow. Native session IDs, which a driver assigns and which are
/// unique only within their source, are green. The two colors keep a
/// stored session and its native counterpart apart when both appear
/// in the same terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdKind {
    Gage,
    Native,
}

impl IdKind {
    /// Styled display of an ID split at its disambiguating prefix:
    /// the bright shade over `prefix`, the dark shade over `tail`
    pub fn style(self, prefix: &str, tail: &str) -> String {
        match self {
            IdKind::Gage => format!(
                "{}{}",
                style(prefix).yellow().bright(),
                style(tail).yellow()
            ),
            IdKind::Native => format!("{}{}", style(prefix).green().bright(), style(tail).green()),
        }
    }
}

/// Renders IDs as short, jj-style displays where the disambiguating
/// prefix is the bright shade of the kind's color and the rest of the
/// shown ID is the dark shade.
///
/// Construct with the full peer set for the entity kind — i.e. every ID
/// that the corresponding `get(id_prefix)` lookup resolves against, not
/// just the rows currently shown. This keeps the highlighted prefix in
/// lockstep with what the CLI actually accepts as a prefix arg.
pub struct IdHighlighter {
    /// Peers sorted lexicographically so neighbors bound the longest
    /// common prefix with any query id.
    sorted_peers: Vec<String>,
    kind: IdKind,
}

impl IdHighlighter {
    /// Highlighter for Gage object IDs
    pub fn new(peers: Vec<String>) -> Self {
        Self::with_kind(peers, IdKind::Gage)
    }

    /// Highlighter for IDs of `kind`
    pub fn with_kind(peers: Vec<String>, kind: IdKind) -> Self {
        let mut sorted_peers = peers;
        sorted_peers.sort_unstable();
        Self { sorted_peers, kind }
    }

    /// Length in characters of the shortest prefix of `id` that resolves
    /// uniquely against the peer set. Always at least 1. May exceed the
    /// character count of `id` only when a peer duplicates `id`
    /// character-for-character, which is not a case Gage constructs.
    pub fn unique_prefix_len(&self, id: &str) -> usize {
        let pos = match self.sorted_peers.binary_search_by(|p| p.as_str().cmp(id)) {
            Ok(i) => i,
            Err(i) => i,
        };
        let lo = pos.saturating_sub(1);
        let hi = (pos + 2).min(self.sorted_peers.len());
        let mut max_common = 0usize;
        for peer in self.sorted_peers.get(lo..hi).into_iter().flatten() {
            if peer == id {
                continue;
            }
            let common = id
                .chars()
                .zip(peer.chars())
                .take_while(|(a, b)| a == b)
                .count();
            max_common = max_common.max(common);
        }
        max_common + 1
    }

    /// Styled 8-char short display: the bright shade up to the unique
    /// prefix, the dark shade for the tail. If the unique prefix
    /// reaches or exceeds the 8-char display length, the whole short
    /// display is bright — the display is ambiguous and the caller may
    /// want to widen it (see `full`), but the coloring at least signals
    /// which chars carry disambiguation weight.
    pub fn short(&self, id: &str) -> String {
        self.styled(id, short_uuid(id))
    }

    /// Styled full-length display: the bright shade up to the unique
    /// prefix, the dark shade for the tail. If the unique prefix
    /// reaches or exceeds the ID length — only when a peer duplicates
    /// the ID character-for-character, which Gage does not construct —
    /// the whole ID is bright.
    pub fn full(&self, id: &str) -> String {
        self.styled(id, id)
    }

    fn styled(&self, id: &str, display: &str) -> String {
        let split_chars = self.unique_prefix_len(id);
        let split_bytes = display
            .char_indices()
            .nth(split_chars)
            .map(|(i, _)| i)
            .unwrap_or(display.len());
        let (prefix, tail) = display.split_at(split_bytes);
        self.kind.style(prefix, tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(styled: &str) -> String {
        // Strip ANSI escape sequences for content assertions.
        let mut out = String::with_capacity(styled.len());
        let mut in_esc = false;
        for c in styled.chars() {
            if in_esc {
                if c.is_ascii_alphabetic() {
                    in_esc = false;
                }
            } else if c == '\x1b' {
                in_esc = true;
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn unique_prefix_length_matches_neighbors() {
        let h = IdHighlighter::new(vec![
            "01k2h4t9aaaa".into(),
            "01k9m2b0bbbb".into(),
            "03aqzz1xcccc".into(),
            "7ffb2d7gdddd".into(),
            "bc0011x2eeee".into(),
        ]);
        // Shared "01k" with sibling — need 4 chars ("01k2" / "01k9") to disambiguate.
        assert_eq!(h.unique_prefix_len("01k2h4t9aaaa"), 4);
        assert_eq!(h.unique_prefix_len("01k9m2b0bbbb"), 4);
        // Shares just "0" with the "01..." pair — need 2 chars.
        assert_eq!(h.unique_prefix_len("03aqzz1xcccc"), 2);
        // No shared prefix with any neighbor — 1 char is enough.
        assert_eq!(h.unique_prefix_len("7ffb2d7gdddd"), 1);
        assert_eq!(h.unique_prefix_len("bc0011x2eeee"), 1);
    }

    #[test]
    fn single_peer_needs_one_char() {
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into()]);
        assert_eq!(h.unique_prefix_len("01k2h4t9aaaa"), 1);
    }

    #[test]
    fn empty_peer_set_needs_one_char() {
        let h = IdHighlighter::new(vec![]);
        assert_eq!(h.unique_prefix_len("01k2h4t9aaaa"), 1);
    }

    #[test]
    fn query_id_not_in_peers() {
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into(), "01k9m2b0bbbb".into()]);
        // "01k5..." sits between the two — shares "01k", needs 4 chars.
        assert_eq!(h.unique_prefix_len("01k5xxxxxxxx"), 4);
    }

    #[test]
    fn short_display_length_and_content() {
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into(), "01k9m2b0bbbb".into()]);
        let out = h.short("01k2h4t9aaaa");
        assert_eq!(plain(&out), "01k2h4t9");
    }

    /// Builds the expected styled string by calling `console::style` the same
    /// way `styled()` does. If `console` ever changes its emit format both
    /// sides move together, so the tests stay meaningful without hard-coding
    /// escape sequences.
    fn expected_styled(prefix: &str, tail: &str) -> String {
        format!(
            "{}{}",
            console::style(prefix).yellow().bright(),
            console::style(tail).yellow(),
        )
    }

    #[test]
    fn short_splits_at_unique_prefix() {
        console::set_colors_enabled(true);
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into(), "01k9m2b0bbbb".into()]);
        // Shared "01k" needs 4 chars ("01k2") to disambiguate.
        assert_eq!(h.short("01k2h4t9aaaa"), expected_styled("01k2", "h4t9"));
    }

    #[test]
    fn full_splits_at_unique_prefix() {
        console::set_colors_enabled(true);
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into(), "01k9m2b0bbbb".into()]);
        // Same split boundary, applied over the whole id rather than short().
        assert_eq!(h.full("01k2h4t9aaaa"), expected_styled("01k2", "h4t9aaaa"),);
    }

    #[test]
    fn short_all_bright_when_prefix_reaches_display_length() {
        console::set_colors_enabled(true);
        // Peer shares the full 8-char short display; unique_prefix_len is 9,
        // clamped by min(display.len()) to 8 — every visible char is bright.
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into(), "01k2h4t9bbbb".into()]);
        assert_eq!(h.short("01k2h4t9aaaa"), expected_styled("01k2h4t9", ""));
    }

    #[test]
    fn short_single_peer_is_one_bright_char() {
        console::set_colors_enabled(true);
        let h = IdHighlighter::new(vec!["01k2h4t9aaaa".into()]);
        // No neighbor to disambiguate against: unique_prefix_len is 1.
        assert_eq!(h.short("01k2h4t9aaaa"), expected_styled("0", "1k2h4t9"));
    }

    #[test]
    fn native_kind_styles_green() {
        console::set_colors_enabled(true);
        let h = IdHighlighter::with_kind(
            vec!["4045c48b-aaaa".into(), "4045c48b-bbbb".into()],
            IdKind::Native,
        );
        // Shared "4045c48b-" needs 10 chars to disambiguate.
        let expected = format!(
            "{}{}",
            console::style("4045c48b-a").green().bright(),
            console::style("aaa").green(),
        );
        assert_eq!(h.full("4045c48b-aaaa"), expected);
    }

    #[test]
    fn full_handles_multibyte_shared_prefix() {
        console::set_colors_enabled(true);
        // "\u{1F600}" (😀) and "\u{1F900}" (🤀) are both 4-byte UTF-8
        // sequences that share their first two leading bytes (f0 9f).
        // Under a byte-count implementation, max_common would be 2 and
        // split_at(3) would land inside the 4-byte emoji and panic. Under
        // char counting, the emojis differ at char position 0, so the
        // split lands on the char boundary between the emoji and "xy".
        let h = IdHighlighter::new(vec!["\u{1F600}xy".into(), "\u{1F900}xy".into()]);
        assert_eq!(h.full("\u{1F600}xy"), expected_styled("\u{1F600}", "xy"));
    }
}
