//! Gage URLs: `scheme:body[#fragment]`, and the line selection
//! fragment that session URLs may carry. The grammar is the README's
//! "Gage URLs" section. Parsing is syntactic; what a body names is the
//! owning module's business.

use std::fmt;
use std::str::FromStr;

use crate::StoreError;

/// A Gage URL split into its parts, borrowed from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GageUrl<'a> {
    pub scheme: &'a str,
    pub body: &'a str,
    pub fragment: Option<&'a str>,
}

/// Split `url` into scheme, body, and fragment. The scheme is an
/// ASCII letter followed by letters, digits, `+`, `-`, or `.`; a
/// missing or malformed scheme is [`StoreError::BadUrl`].
pub fn parse(url: &str) -> Result<GageUrl<'_>, StoreError> {
    let (scheme, rest) = url
        .split_once(':')
        .ok_or_else(|| StoreError::BadUrl(url.to_string()))?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid {
        return Err(StoreError::BadUrl(url.to_string()));
    }
    let (body, fragment) = match rest.split_once('#') {
        Some((body, fragment)) => (body, Some(fragment)),
        None => (rest, None),
    };
    Ok(GageUrl {
        scheme,
        body,
        fragment,
    })
}

/// Split a session origin, `session:<id>@<commit>`, into the original
/// session id and the commit. This is the inverse of the origin a
/// publish records. A missing `session:` prefix or `@` separator is
/// [`StoreError::BadUrl`].
pub fn parse_session_origin(origin: &str) -> Result<(&str, &str), StoreError> {
    origin
        .strip_prefix("session:")
        .and_then(|rest| rest.split_once('@'))
        .ok_or_else(|| StoreError::BadUrl(origin.to_string()))
}

/// Check a line selection fragment: parts joined by `,`, each a
/// 1-based line or an inclusive `start-end` range with `start <= end`.
pub fn validate_line_selection(fragment: &str) -> Result<(), StoreError> {
    fragment.parse::<LineSelection>().map(|_| ())
}

/// A line selection as a session URL fragment carries it: one or more
/// 1-based lines or inclusive `start-end` ranges, in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineSelection {
    ranges: Vec<(u64, u64)>,
}

impl LineSelection {
    /// The selected `(start, end)` ranges, inclusive; a single line is
    /// a range of one.
    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    pub fn contains(&self, line: u64) -> bool {
        self.ranges
            .iter()
            .any(|&(start, end)| (start..=end).contains(&line))
    }
}

impl FromStr for LineSelection {
    type Err = StoreError;

    fn from_str(fragment: &str) -> Result<Self, StoreError> {
        let bad = || StoreError::BadLineSelection(fragment.to_string());
        if fragment.is_empty() {
            return Err(bad());
        }
        let mut ranges = Vec::new();
        for part in fragment.split(',') {
            let (start, end) = match part.split_once('-') {
                Some((s, e)) => (s, Some(e)),
                None => (part, None),
            };
            let start = parse_line(start).ok_or_else(bad)?;
            let end = match end {
                Some(end) => parse_line(end).ok_or_else(bad)?,
                None => start,
            };
            if end < start {
                return Err(bad());
            }
            ranges.push((start, end));
        }
        Ok(Self { ranges })
    }
}

impl fmt::Display for LineSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, &(start, end)) in self.ranges.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            if start == end {
                write!(f, "{start}")?;
            } else {
                write!(f, "{start}-{end}")?;
            }
        }
        Ok(())
    }
}

fn parse_line(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok().filter(|&n| n >= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_scheme_body_and_fragment() {
        let url = parse("session:abc#12-20,31").unwrap();
        assert_eq!(url.scheme, "session");
        assert_eq!(url.body, "abc");
        assert_eq!(url.fragment, Some("12-20,31"));
        let url = parse("task:finding-issues:issues").unwrap();
        assert_eq!(url.scheme, "task");
        assert_eq!(url.body, "finding-issues:issues");
        assert_eq!(url.fragment, None);
        assert_eq!(parse("session+task:x").unwrap().scheme, "session+task");
    }

    #[test]
    fn parse_rejects_missing_or_malformed_scheme() {
        for bad in ["abc", "", ":abc", "1x:abc", "a b:c"] {
            assert!(
                matches!(parse(bad), Err(StoreError::BadUrl(u)) if u == bad),
                "{bad}"
            );
        }
    }

    #[test]
    fn line_selection_parses_and_prints_its_ranges() {
        let sel: LineSelection = "100-200,315,7-7".parse().unwrap();
        assert_eq!(sel.ranges(), [(100, 200), (315, 315), (7, 7)]);
        assert_eq!(sel.to_string(), "100-200,315,7");
        assert!(sel.contains(150) && sel.contains(315) && sel.contains(7));
        assert!(!sel.contains(99) && !sel.contains(316));
    }

    #[test]
    fn line_selection_grammar() {
        for ok in ["1", "100-200", "100-200,315", "7-7"] {
            assert!(validate_line_selection(ok).is_ok(), "{ok}");
        }
        for bad in ["", "0", "a", "10-", "-10", "20-10", "1,,2", "1 -2", "1-2-3"] {
            assert!(
                matches!(
                    validate_line_selection(bad),
                    Err(StoreError::BadLineSelection(f)) if f == bad
                ),
                "{bad}"
            );
        }
    }
}
