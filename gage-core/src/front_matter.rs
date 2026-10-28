//! The `+++` front matter of a markdown file: a TOML block between
//! two fence lines at the top of the text, before the body.

use std::fmt;

/// A front matter block that opens without closing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnclosedFrontMatter;

impl fmt::Display for UnclosedFrontMatter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("front matter opened with a `+++` line is not closed by one")
    }
}

impl std::error::Error for UnclosedFrontMatter {}

/// The TOML text of the front matter, when `text` opens with a `+++`
/// line, and the body after it. A text without front matter is all
/// body. The body keeps its own surrounding whitespace.
pub fn split(text: &str) -> Result<(Option<&str>, &str), UnclosedFrontMatter> {
    let Some(after_open) = text.strip_prefix("+++\n") else {
        return Ok((None, text));
    };
    let (toml, body) = after_open
        .split_once("\n+++\n")
        .or_else(|| after_open.strip_suffix("\n+++").map(|toml| (toml, "")))
        .ok_or(UnclosedFrontMatter)?;
    Ok((Some(toml), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_block_splits_from_the_body() {
        let text = "+++\nname = \"x\"\n+++\n\nBody line.\n";
        assert_eq!(split(text), Ok((Some("name = \"x\""), "\nBody line.\n")));
    }

    #[test]
    fn a_text_without_a_fence_is_all_body() {
        let text = "Body line.\n\n{{ prompt }}\n";
        assert_eq!(split(text), Ok((None, text)));
    }

    #[test]
    fn a_closing_fence_at_the_end_leaves_an_empty_body() {
        assert_eq!(split("+++\na = 1\n+++"), Ok((Some("a = 1"), "")));
    }

    #[test]
    fn an_unclosed_fence_is_an_error() {
        assert_eq!(split("+++\na = 1\n"), Err(UnclosedFrontMatter));
    }
}
