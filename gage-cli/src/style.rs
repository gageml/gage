use console::style;
use gage_core::uuid::short_uuid;
use tabled::settings::Color;

pub fn spinner(message: &str) -> indicatif::ProgressBar {
    let spinner = indicatif::ProgressBar::new_spinner();
    spinner
        .set_style(indicatif::ProgressStyle::with_template("{spinner:.magenta}  {msg}").unwrap());
    spinner.set_message(message.to_string());
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner
}

pub fn dim() -> Color {
    Color::new("\x1b[2m", "\x1b[22m")
}

pub fn dim_italic() -> Color {
    Color::new("\x1b[2;3m", "\x1b[22;23m")
}

/// Renders IDs as short, jj-style displays where the disambiguating
/// prefix is bright yellow and the rest of the shown ID is dark yellow.
///
/// Construct with the full peer set for the entity kind — i.e. every ID
/// that the corresponding `get(id_prefix)` lookup resolves against, not
/// just the rows currently shown. This keeps the highlighted prefix in
/// lockstep with what the CLI actually accepts as a prefix arg.
pub struct IdHighlighter {
    /// Peers sorted lexicographically so neighbors bound the longest
    /// common prefix with any query id.
    sorted_peers: Vec<String>,
}

impl IdHighlighter {
    pub fn new(peers: Vec<String>) -> Self {
        let mut sorted_peers = peers;
        sorted_peers.sort_unstable();
        Self { sorted_peers }
    }

    /// Length of the shortest prefix of `id` that resolves uniquely
    /// against the peer set. Always at least 1. May exceed `id.len()`
    /// only when a peer duplicates `id` byte-for-byte, which is not a
    /// case Gage constructs.
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
                .bytes()
                .zip(peer.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            max_common = max_common.max(common);
        }
        max_common + 1
    }

    /// Styled 8-char short display: bright yellow up to the unique
    /// prefix, dark yellow for the tail. If the unique prefix reaches
    /// or exceeds the shown length, the whole short display is bright
    /// yellow — the display is ambiguous and the caller may want to
    /// widen it, but the coloring at least signals which chars carry
    /// disambiguation weight.
    pub fn short(&self, id: &str) -> String {
        self.styled(id, short_uuid(id))
    }

    /// Styled full-length display: same two-tone treatment as `short`,
    /// applied over the entire ID.
    pub fn full(&self, id: &str) -> String {
        self.styled(id, id)
    }

    fn styled(&self, id: &str, display: &str) -> String {
        let split = self.unique_prefix_len(id).min(display.len());
        let (prefix, tail) = display.split_at(split);
        format!(
            "{}{}",
            style(prefix).yellow().bright(),
            style(tail).yellow(),
        )
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
}
