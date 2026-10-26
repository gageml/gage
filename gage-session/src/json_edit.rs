//! Order-preserving edits to the values of a JSON document: a string
//! value replaced, or any value left out.
//!
//! `serde_json::Value` sorts object keys, so an edit goes through the
//! deserializer instead: each container on the path to an edited
//! value is decoded with its members as raw text, the edited leaf is
//! replaced or left out, and the container is written back in its
//! original member order. A sibling off the path is reproduced byte
//! for byte; the punctuation and whitespace of a container on the
//! path are written compactly. A document with no edits is returned
//! as given.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

/// An edit that names no value of the document it can apply to.
#[derive(Debug)]
pub struct EditError {
    pub pointer: String,
    pub message: String,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "edit at {}: {}", self.pointer, self.message)
    }
}

impl Error for EditError {}

/// `json` with each string value named by a JSON Pointer in `edits`
/// replaced by the paired text.
pub fn edit_strings(json: &str, edits: &[(&str, &str)]) -> Result<String, EditError> {
    if edits.is_empty() {
        return Ok(json.to_string());
    }
    let edits: Vec<Edit<'_>> = edits
        .iter()
        .map(|(pointer, text)| Edit {
            pointer,
            tokens: tokens(pointer),
            op: Op::Replace(text),
        })
        .collect();
    rewrite(json, &edits)
}

/// `json` with each value named by a JSON Pointer in `pointers` left
/// out: an object member goes with its name, and an array element
/// goes with the elements after it moving up. Every pointer names a
/// position in the document as given, so pointers into one array do
/// not shift each other.
pub fn delete_values(json: &str, pointers: &[&str]) -> Result<String, EditError> {
    if pointers.is_empty() {
        return Ok(json.to_string());
    }
    let edits: Vec<Edit<'_>> = pointers
        .iter()
        .map(|pointer| Edit {
            pointer,
            tokens: tokens(pointer),
            op: Op::Delete,
        })
        .collect();
    rewrite(json, &edits)
}

struct Edit<'a> {
    pointer: &'a str,
    tokens: Vec<String>,
    op: Op<'a>,
}

enum Op<'a> {
    /// Replace the string value with the text
    Replace(&'a str),
    /// Leave the value out of its container
    Delete,
}

/// The reference tokens of a JSON Pointer, unescaped.
fn tokens(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// `raw` with `edits` applied, every edit's tokens being relative to
/// `raw`.
fn rewrite(raw: &str, edits: &[Edit<'_>]) -> Result<String, EditError> {
    if edits.is_empty() {
        return Ok(raw.to_string());
    }
    if let Some(leaf) = edits.iter().find(|e| e.tokens.is_empty()) {
        if edits.len() > 1 {
            return Err(error(leaf.pointer, "overlaps another edit"));
        }
        let Op::Replace(text) = leaf.op else {
            return Err(error(
                leaf.pointer,
                "the document itself cannot be left out",
            ));
        };
        let raw: &RawValue = serde_json::from_str(raw).map_err(|e| error(leaf.pointer, e))?;
        if !raw.get().trim_start().starts_with('"') {
            return Err(error(leaf.pointer, "value is not a string"));
        }
        return serde_json::to_string(text).map_err(|e| error(leaf.pointer, e));
    }
    let mut by_member: BTreeMap<&str, Vec<Edit<'_>>> = BTreeMap::new();
    for e in edits {
        let (head, rest) = e
            .tokens
            .split_first()
            .expect("leaf edits were handled above");
        by_member.entry(head.as_str()).or_default().push(Edit {
            pointer: e.pointer,
            tokens: rest.to_vec(),
            op: match e.op {
                Op::Replace(text) => Op::Replace(text),
                Op::Delete => Op::Delete,
            },
        });
    }
    let first = edits.first().expect("edits is not empty");
    match raw.trim_start().chars().next() {
        Some('{') => {
            let object: OrderedObject =
                serde_json::from_str(raw).map_err(|e| error(first.pointer, e))?;
            let mut out = String::with_capacity(raw.len());
            out.push('{');
            let mut written = 0;
            for (key, value) in &object.0 {
                let inner = by_member.remove(key.as_str());
                if inner.as_deref().is_some_and(deletes_whole) {
                    continue;
                }
                if written > 0 {
                    out.push(',');
                }
                written += 1;
                out.push_str(&serde_json::to_string(key).map_err(|e| error(first.pointer, e))?);
                out.push(':');
                match inner {
                    Some(inner) => out.push_str(&rewrite(value.get(), &inner)?),
                    None => out.push_str(value.get()),
                }
            }
            out.push('}');
            if let Some((member, inner)) = by_member.into_iter().next() {
                let missing = inner.first().expect("a member group is not empty");
                return Err(error(missing.pointer, format!("no member {member:?}")));
            }
            Ok(out)
        }
        Some('[') => {
            let items: Vec<Box<RawValue>> =
                serde_json::from_str(raw).map_err(|e| error(first.pointer, e))?;
            let mut out = String::with_capacity(raw.len());
            out.push('[');
            let mut written = 0;
            for (i, value) in items.iter().enumerate() {
                let index = i.to_string();
                let inner = by_member.remove(index.as_str());
                if inner.as_deref().is_some_and(deletes_whole) {
                    continue;
                }
                if written > 0 {
                    out.push(',');
                }
                written += 1;
                match inner {
                    Some(inner) => out.push_str(&rewrite(value.get(), &inner)?),
                    None => out.push_str(value.get()),
                }
            }
            out.push(']');
            if let Some((member, inner)) = by_member.into_iter().next() {
                let missing = inner.first().expect("a member group is not empty");
                return Err(error(missing.pointer, format!("no element {member}")));
            }
            Ok(out)
        }
        _ => Err(error(first.pointer, "value is not a container")),
    }
}

/// Whether `edits`, all relative to one member, is the one edit that
/// leaves the member out.
fn deletes_whole(edits: &[Edit<'_>]) -> bool {
    matches!(edits, [Edit { tokens, op: Op::Delete, .. }] if tokens.is_empty())
}

fn error(pointer: &str, message: impl fmt::Display) -> EditError {
    EditError {
        pointer: pointer.to_string(),
        message: message.to_string(),
    }
}

/// An object's members in written order, each value as raw text.
struct OrderedObject(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for OrderedObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;

        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = OrderedObject;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut members = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    members.push((key, value));
                }
                Ok(OrderedObject(members))
            }
        }

        deserializer.deserialize_map(ObjectVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"zeta":"z","message":{"content":[{"type":"text","text":"hello world"},{"type":"tool_use","input":{"command":"ls  -la"}}]},"n": 1.50,"alpha":"a"}"#;

    #[test]
    fn edits_replace_leaves_and_keep_everything_else_byte_for_byte() {
        let out = edit_strings(
            LINE,
            &[
                ("/message/content/0/text", "hello [REDACTED]"),
                ("/alpha", "A"),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            r#"{"zeta":"z","message":{"content":[{"type":"text","text":"hello [REDACTED]"},{"type":"tool_use","input":{"command":"ls  -la"}}]},"n":1.50,"alpha":"A"}"#
        );
    }

    #[test]
    fn no_edits_returns_the_document_as_given() {
        assert_eq!(edit_strings(LINE, &[]).unwrap(), LINE);
        assert_eq!(delete_values(LINE, &[]).unwrap(), LINE);
    }

    /// A deleted member goes with its name and its comma, a deleted
    /// element moves the ones after it up, and pointers into one
    /// array name positions in the document as given.
    #[test]
    fn deletes_leave_out_members_and_elements() {
        let out = delete_values(LINE, &["/zeta", "/message/content/0", "/n"]).unwrap();
        assert_eq!(
            out,
            r#"{"message":{"content":[{"type":"tool_use","input":{"command":"ls  -la"}}]},"alpha":"a"}"#
        );
        let out = delete_values(
            r#"{"a":["x","y","z"],"b":{"c":1}}"#,
            &["/a/0", "/a/2", "/b/c"],
        )
        .unwrap();
        assert_eq!(out, r#"{"a":["y"],"b":{}}"#);
        let last = delete_values(r#"{"a":1,"b":2}"#, &["/b"]).unwrap();
        assert_eq!(last, r#"{"a":1}"#);
    }

    /// A delete and a replace under one member apply together, and a
    /// delete that overlaps a deeper edit or names the document is an
    /// error.
    #[test]
    fn deletes_combine_with_replaces_and_refuse_overlaps() {
        let replaced = edit_strings(LINE, &[("/alpha", "A")]).unwrap();
        let out = delete_values(&replaced, &["/zeta"]).unwrap();
        assert!(out.starts_with(r#"{"message":"#), "{out}");
        assert!(out.ends_with(r#""alpha":"A"}"#), "{out}");
        let missing = delete_values(LINE, &["/message/nope"]).unwrap_err();
        assert!(missing.message.contains("no member"), "{missing}");
        let root = delete_values(LINE, &[""]).unwrap_err();
        assert!(root.message.contains("document itself"), "{root}");
    }

    #[test]
    fn escaped_member_names_resolve() {
        let out = edit_strings(
            r#"{"a/b":"x","c~d":"y"}"#,
            &[("/a~1b", "1"), ("/c~0d", "2")],
        )
        .unwrap();
        assert_eq!(out, r#"{"a/b":"1","c~d":"2"}"#);
    }

    #[test]
    fn a_missing_member_or_a_non_string_leaf_is_an_error() {
        let missing = edit_strings(LINE, &[("/message/nope", "x")]).unwrap_err();
        assert_eq!(missing.pointer, "/message/nope");
        assert!(missing.message.contains("no member"), "{missing}");
        let number = edit_strings(LINE, &[("/n", "x")]).unwrap_err();
        assert!(number.message.contains("not a string"), "{number}");
        let deep = edit_strings(LINE, &[("/zeta/0", "x")]).unwrap_err();
        assert!(deep.message.contains("not a container"), "{deep}");
    }
}
