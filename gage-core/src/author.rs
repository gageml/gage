//! The author value convention.
//!
//! An author names who wrote a db item (note, issue, comment) in a
//! URI-like form:
//!
//! ```text
//! {scheme}:{identity}[?{attr}={value}[&{attr}={value}...]]
//! ```
//!
//! - `scheme` names the kind of principal: `user` for humans, `agent`
//!   for model-driven writers.
//! - `identity` names the principal within the scheme: a username, a
//!   scanner name, a client name.
//! - Attributes qualify the acting occurrence, e.g. `call` (the
//!   tool-use id of the writing call), `scan` (the scan run an
//!   annotator acted in), `ver` (a client version).
//!
//! Attribute values are URL percent-encoded, and attributes are sorted
//! lexicographically by name, so equal qualifications compare equal as
//! strings.

use std::fmt::Write;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

/// Characters percent-encoded in attribute values: everything a URL
/// query reserves plus the delimiters this format assigns meaning to.
const VALUE_ENCODE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'<')
    .add(b'>')
    .add(b'=')
    .add(b'?');

/// Compose an author value from its parts. Attributes are rendered
/// sorted by name with percent-encoded values; an empty attribute list
/// yields the bare `{scheme}:{identity}` form.
pub fn compose(scheme: &str, identity: &str, attrs: &[(&str, &str)]) -> String {
    let mut author = format!("{scheme}:{identity}");
    let mut attrs: Vec<&(&str, &str)> = attrs.iter().collect();
    attrs.sort_by_key(|(name, _)| *name);
    for (i, (name, value)) in attrs.into_iter().enumerate() {
        let sep = if i == 0 { '?' } else { '&' };
        let value = utf8_percent_encode(value, VALUE_ENCODE);
        write!(author, "{sep}{name}={value}").unwrap();
    }
    author
}

/// Add an attribute to an existing author value, keeping the attribute
/// list sorted by name. The base's existing attribute values pass
/// through verbatim (they were encoded when composed).
pub fn append_attr(base: &str, name: &str, value: &str) -> String {
    let (identity, query) = match base.split_once('?') {
        Some((identity, query)) => (identity, Some(query)),
        None => (base, None),
    };
    let encoded = utf8_percent_encode(value, VALUE_ENCODE).to_string();
    let mut attrs: Vec<(&str, &str)> = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .collect();
    attrs.push((name, &encoded));
    attrs.sort_by_key(|(name, _)| *name);
    let mut author = identity.to_string();
    for (i, (name, value)) in attrs.into_iter().enumerate() {
        let sep = if i == 0 { '?' } else { '&' };
        write!(author, "{sep}{name}={value}").unwrap();
    }
    author
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_bare() {
        assert_eq!(compose("user", "garrett", &[]), "user:garrett");
    }

    #[test]
    fn compose_sorts_attrs() {
        assert_eq!(
            compose("agent", "open-code", &[("ver", "2"), ("call", "abc")]),
            "agent:open-code?call=abc&ver=2"
        );
    }

    #[test]
    fn compose_encodes_values() {
        assert_eq!(
            compose("agent", "x", &[("ver", "a&b=c")]),
            "agent:x?ver=a%26b%3Dc"
        );
    }

    #[test]
    fn append_to_bare_base() {
        assert_eq!(
            append_attr("agent:general", "call", "t1"),
            "agent:general?call=t1"
        );
    }

    #[test]
    fn append_keeps_sorted_order() {
        assert_eq!(
            append_attr("agent:open-code?scan=s1", "call", "t1"),
            "agent:open-code?call=t1&scan=s1"
        );
    }
}
