//! Rune values as serde data for template contexts.

use std::fmt;

use rune::runtime::{Object, Value};
use serde::ser::{Error as _, Serialize, SerializeMap, SerializeSeq, Serializer};

use crate::note::Note;
use crate::row::{Entry, Message};

/// A Rune object as a serde map, for template contexts.
pub struct SerObject<'a>(pub &'a Object);

impl Serialize for SerObject<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        serialize_object(self.0, None, ser)
    }
}

fn serialize_object<S: Serializer>(
    obj: &Object,
    path: Option<&Path<'_>>,
    ser: S,
) -> Result<S::Ok, S::Error> {
    let mut map = ser.serialize_map(Some(obj.len()))?;
    for (k, v) in obj.iter() {
        let path = Path {
            parent: path,
            segment: Segment::Key(k.as_str()),
        };
        map.serialize_entry(
            k.as_str(),
            &SerValue {
                value: v,
                path: &path,
            },
        )?;
    }
    map.end()
}

/// The location of a value in the context, rendered only in an error
struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    segment: Segment<'a>,
}

enum Segment<'a> {
    Key(&'a str),
    Index(usize),
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(parent) = self.parent {
            write!(f, "{parent}")?;
        }
        match self.segment {
            Segment::Key(k) if self.parent.is_some() => write!(f, ".{k}"),
            Segment::Key(k) => write!(f, "{k}"),
            Segment::Index(i) => write!(f, "[{i}]"),
        }
    }
}

struct SerValue<'a> {
    value: &'a Value,
    path: &'a Path<'a>,
}

impl Serialize for SerValue<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let v = self.value;
        if v.into_unit().is_ok() {
            return ser.serialize_none();
        }
        if let Ok(s) = v.borrow_string_ref() {
            return ser.serialize_str(&s);
        }
        if let Ok(i) = v.as_integer::<i64>() {
            return ser.serialize_i64(i);
        }
        if let Ok(f) = v.as_float() {
            return ser.serialize_f64(f);
        }
        if let Ok(b) = v.as_bool() {
            return ser.serialize_bool(b);
        }
        if let Ok(msg) = v.borrow_ref::<Message>() {
            return serialize_object(&msg.inner, Some(self.path), ser);
        }
        if let Ok(entry) = v.borrow_ref::<Entry>() {
            return serialize_object(&entry.inner, Some(self.path), ser);
        }
        if let Ok(note) = v.borrow_ref::<Note>() {
            return self.serialize_note(&note, ser);
        }
        if let Ok(vec) = v.borrow_ref::<rune::runtime::Vec>() {
            let mut seq = ser.serialize_seq(Some(vec.len()))?;
            for (i, item) in vec.iter().enumerate() {
                let path = Path {
                    parent: Some(self.path),
                    segment: Segment::Index(i),
                };
                seq.serialize_element(&SerValue {
                    value: item,
                    path: &path,
                })?;
            }
            return seq.end();
        }
        if let Ok(obj) = v.borrow_ref::<Object>() {
            return serialize_object(&obj, Some(self.path), ser);
        }
        if let Ok(Some(inner)) = v.borrow_ref::<Option<Value>>().as_deref() {
            return SerValue {
                value: inner,
                path: self.path,
            }
            .serialize(ser);
        }
        let ty = v.type_info().to_string();
        let ty = ty.strip_prefix("::").unwrap_or(&ty);
        Err(S::Error::custom(format!(
            "{} holds a {ty} value, which cannot be rendered in a template context",
            self.path
        )))
    }
}

impl SerValue<'_> {
    /// The fields a script reads on a note, under the same names
    fn serialize_note<S: Serializer>(&self, note: &Note, ser: S) -> Result<S::Ok, S::Error> {
        let mut map = ser.serialize_map(Some(7))?;
        map.serialize_entry("id", &note.id)?;
        map.serialize_entry("name", &note.name)?;
        map.serialize_entry("value", &self.field("value", &note.value))?;
        map.serialize_entry("author", &note.author)?;
        map.serialize_entry("target", &note.target)?;
        map.serialize_entry("metadata", &self.field("metadata", &note.metadata))?;
        map.serialize_entry("created", &note.created)?;
        map.end()
    }

    fn field<'b>(&'b self, key: &'b str, value: &'b Value) -> SerField<'b> {
        SerField {
            value,
            path: Path {
                parent: Some(self.path),
                segment: Segment::Key(key),
            },
        }
    }
}

/// A nested value that owns its path segment, for fields read off a
/// Rust struct rather than a Rune object
struct SerField<'a> {
    value: &'a Value,
    path: Path<'a>,
}

impl Serialize for SerField<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        SerValue {
            value: self.value,
            path: &self.path,
        }
        .serialize(ser)
    }
}
