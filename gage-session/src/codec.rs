//! The entry codec: a session entry decoded from its native bytes into
//! an [`EntryValue`], and encoded back. An `EntryValue` is JSON whose
//! object keys keep the order the native content carries them, which
//! `serde_json::Value` does not, so an entry that is decoded, edited at
//! one value, and encoded again differs from the original only at
//! that value.

use std::fmt;

use indexmap::IndexMap;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};

use crate::DriverError;
use crate::text_block::TextDescriptor;

/// The key `serde_json` uses to carry a number's text through
/// `deserialize_any` when its `arbitrary_precision` feature is on.
const NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// The `type` of the entry a codec stands in for one native entry it
/// cannot decode: `{"type":"$gage-invalid-entry","error":..,"raw":..}`.
/// The entry keeps the native line's place in the sequence, `raw` is
/// the line's text, and `encode` writes `raw` back unchanged. A codec
/// returns it only for a corruption it recognizes; any other input it
/// cannot decode is an error.
pub const INVALID_ENTRY_TYPE: &str = "$gage-invalid-entry";

/// A session entry as its codec decodes it, with object keys in
/// document order.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryValue {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<EntryValue>),
    Object(IndexMap<String, EntryValue>),
}

/// One entry of a stored session, decoded, with its line.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedEntry {
    /// 1-based line in the session's native content
    pub line: u32,
    pub value: EntryValue,
}

/// Decodes and encodes the entries of one content format, and
/// describes the strings it writes. A driver returns one per content
/// format it reads.
pub trait EntryCodec: Send + Sync {
    /// The native bytes of one entry as a value. Bytes in a corrupt
    /// state the codec recognizes decode to an [`INVALID_ENTRY_TYPE`]
    /// entry holding them; bytes it cannot decode otherwise are an
    /// error, and a session holding such bytes is not read.
    fn decode(&self, raw: &[u8]) -> Result<EntryValue, DriverError>;
    /// The native bytes of `value`; the `raw` text of an
    /// [`INVALID_ENTRY_TYPE`] entry.
    fn encode(&self, value: &EntryValue) -> Result<Vec<u8>, DriverError>;
    /// Which strings of this format are not text, and which a typed
    /// set selects.
    fn descriptor(&self) -> &TextDescriptor;
}

impl EntryValue {
    /// The entry that stands in for a native entry that does not
    /// decode: `error` says why, `raw` is its text.
    pub fn invalid_entry(error: &str, raw: &str) -> EntryValue {
        let mut map = IndexMap::new();
        map.insert(
            "type".to_string(),
            EntryValue::String(INVALID_ENTRY_TYPE.into()),
        );
        map.insert("error".to_string(), EntryValue::String(error.into()));
        map.insert("raw".to_string(), EntryValue::String(raw.into()));
        EntryValue::Object(map)
    }

    /// A copy of the value as a `serde_json::Value`, for a value the
    /// caller keeps. `Value::from` moves and is the conversion to
    /// reach for; this one copies every string.
    pub fn to_json_value(&self) -> Value {
        Value::from(self.clone())
    }

    /// The `raw` text of an [`INVALID_ENTRY_TYPE`] entry; `None` for
    /// any other entry.
    pub fn invalid_entry_raw(&self) -> Option<&str> {
        let is_invalid = self.get("type").and_then(EntryValue::as_str) == Some(INVALID_ENTRY_TYPE);
        is_invalid.then(|| self.get("raw").and_then(EntryValue::as_str))?
    }

    /// The member `key` of an object; `None` for another kind of value.
    pub fn get(&self, key: &str) -> Option<&EntryValue> {
        match self {
            EntryValue::Object(map) => map.get(key),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, EntryValue::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            EntryValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            EntryValue::Number(n) => n.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            EntryValue::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            EntryValue::Number(n) => n.as_f64(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            EntryValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<EntryValue>> {
        match self {
            EntryValue::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<EntryValue>> {
        match self {
            EntryValue::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&IndexMap<String, EntryValue>> {
        match self {
            EntryValue::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut IndexMap<String, EntryValue>> {
        match self {
            EntryValue::Object(map) => Some(map),
            _ => None,
        }
    }

    /// The value at the JSON Pointer (RFC 6901) `pointer`; the root
    /// for the empty pointer, `None` when the pointer names nothing.
    pub fn pointer(&self, pointer: &str) -> Option<&EntryValue> {
        let mut current = self;
        for token in pointer_tokens(pointer)? {
            current = match current {
                EntryValue::Object(map) => map.get(token.as_str())?,
                EntryValue::Array(items) => items.get(array_index(&token)?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    pub fn pointer_mut(&mut self, pointer: &str) -> Option<&mut EntryValue> {
        let mut current = self;
        for token in pointer_tokens(pointer)? {
            current = match current {
                EntryValue::Object(map) => map.get_mut(token.as_str())?,
                EntryValue::Array(items) => items.get_mut(array_index(&token)?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Remove the value at `pointer` from its parent and return it;
    /// the siblings keep their order. `None` when the pointer is the
    /// root or names nothing.
    pub fn remove(&mut self, pointer: &str) -> Option<EntryValue> {
        let mut tokens = pointer_tokens(pointer)?;
        let last = tokens.pop()?;
        let mut parent = self;
        for token in tokens {
            parent = match parent {
                EntryValue::Object(map) => map.get_mut(token.as_str())?,
                EntryValue::Array(items) => items.get_mut(array_index(&token)?)?,
                _ => return None,
            };
        }
        match parent {
            EntryValue::Object(map) => map.shift_remove(last.as_str()),
            EntryValue::Array(items) => {
                let index = array_index(&last)?;
                (index < items.len()).then(|| items.remove(index))
            }
            _ => None,
        }
    }

    /// Every string value in document order, each with its JSON
    /// Pointer from the root.
    pub fn strings(&self) -> Vec<(String, &str)> {
        let mut out = Vec::new();
        let mut pointer = String::new();
        self.collect_strings(&mut pointer, &mut out);
        out
    }

    fn collect_strings<'a>(&'a self, pointer: &mut String, out: &mut Vec<(String, &'a str)>) {
        match self {
            EntryValue::String(s) => out.push((pointer.clone(), s.as_str())),
            EntryValue::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let len = pointer.len();
                    pointer.push('/');
                    pointer.push_str(&i.to_string());
                    item.collect_strings(pointer, out);
                    pointer.truncate(len);
                }
            }
            EntryValue::Object(map) => {
                for (key, item) in map {
                    let len = pointer.len();
                    pointer.push('/');
                    push_escaped(pointer, key);
                    item.collect_strings(pointer, out);
                    pointer.truncate(len);
                }
            }
            _ => {}
        }
    }
}

/// The reference tokens of `pointer`, unescaped; `None` for a
/// non-empty pointer that does not start with `/`.
fn pointer_tokens(pointer: &str) -> Option<Vec<String>> {
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    let rest = pointer.strip_prefix('/')?;
    Some(
        rest.split('/')
            .map(|token| token.replace("~1", "/").replace("~0", "~"))
            .collect(),
    )
}

/// The array index a reference token names: digits without a leading
/// zero, as RFC 6901 requires.
fn array_index(token: &str) -> Option<usize> {
    if token.len() > 1 && token.starts_with('0') {
        return None;
    }
    token.parse().ok()
}

fn push_escaped(pointer: &mut String, key: &str) {
    for c in key.chars() {
        match c {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            c => pointer.push(c),
        }
    }
}

impl From<Value> for EntryValue {
    /// A `serde_json::Value` as an entry value. The keys come in the
    /// order the map yields them, which is sorted without
    /// `preserve_order`; the conversion is for a value with no native
    /// order to keep.
    fn from(value: Value) -> Self {
        match value {
            Value::Null => EntryValue::Null,
            Value::Bool(b) => EntryValue::Bool(b),
            Value::Number(n) => EntryValue::Number(n),
            Value::String(s) => EntryValue::String(s),
            Value::Array(items) => EntryValue::Array(items.into_iter().map(Into::into).collect()),
            Value::Object(map) => {
                EntryValue::Object(map.into_iter().map(|(k, v)| (k, v.into())).collect())
            }
        }
    }
}

impl From<EntryValue> for Value {
    /// An entry value as a `serde_json::Value`. Strings and numbers
    /// move; only the containers are rebuilt, and an object's keys
    /// take the map's order, which is sorted without `preserve_order`.
    fn from(value: EntryValue) -> Self {
        match value {
            EntryValue::Null => Value::Null,
            EntryValue::Bool(b) => Value::Bool(b),
            EntryValue::Number(n) => Value::Number(n),
            EntryValue::String(s) => Value::String(s),
            EntryValue::Array(items) => Value::Array(items.into_iter().map(Into::into).collect()),
            EntryValue::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, v.into())).collect())
            }
        }
    }
}

impl fmt::Display for EntryValue {
    /// The compact JSON text of the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `fmt::Error` carries no source. A value of strings and
        // finite numbers serializes, so the error is unreachable.
        let text = serde_json::to_string(self).map_err(|_unreachable| fmt::Error)?;
        f.write_str(&text)
    }
}

impl Serialize for EntryValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            EntryValue::Null => serializer.serialize_unit(),
            EntryValue::Bool(b) => serializer.serialize_bool(*b),
            EntryValue::Number(n) => n.serialize(serializer),
            EntryValue::String(s) => serializer.serialize_str(s),
            EntryValue::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            EntryValue::Object(map) => {
                let mut out = serializer.serialize_map(Some(map.len()))?;
                for (key, item) in map {
                    out.serialize_entry(key, item)?;
                }
                out.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for EntryValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EntryVisitor)
    }
}

struct EntryVisitor;

impl<'de> Visitor<'de> for EntryVisitor {
    type Value = EntryValue;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<EntryValue, E> {
        Ok(EntryValue::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<EntryValue, E> {
        Ok(EntryValue::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<EntryValue, D::Error> {
        EntryValue::deserialize(deserializer)
    }

    fn visit_bool<E: de::Error>(self, b: bool) -> Result<EntryValue, E> {
        Ok(EntryValue::Bool(b))
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<EntryValue, E> {
        Ok(EntryValue::Number(n.into()))
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<EntryValue, E> {
        Ok(EntryValue::Number(n.into()))
    }

    fn visit_f64<E: de::Error>(self, n: f64) -> Result<EntryValue, E> {
        Number::from_f64(n)
            .map(EntryValue::Number)
            .ok_or_else(|| E::custom("a JSON number is finite"))
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<EntryValue, E> {
        Ok(EntryValue::String(s.to_string()))
    }

    fn visit_string<E: de::Error>(self, s: String) -> Result<EntryValue, E> {
        Ok(EntryValue::String(s))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<EntryValue, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(EntryValue::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<EntryValue, A::Error> {
        let mut out: IndexMap<String, EntryValue> = IndexMap::new();
        let Some(first) = map.next_key::<String>()? else {
            return Ok(EntryValue::Object(out));
        };
        if first == NUMBER_TOKEN {
            let text: String = map.next_value()?;
            let number: Number = text.parse().map_err(de::Error::custom)?;
            return Ok(EntryValue::Number(number));
        }
        let value = map.next_value()?;
        out.insert(first, value);
        while let Some((key, value)) = map.next_entry::<String, EntryValue>()? {
            out.insert(key, value);
        }
        Ok(EntryValue::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn decode(text: &str) -> EntryValue {
        serde_json::from_str(text).unwrap()
    }

    /// Keys come out in the order they went in, at every depth, and
    /// the compact text of an unedited value is the input.
    #[test]
    fn a_round_trip_keeps_document_order() {
        let text = r#"{"zeta":{"b":1,"a":[{"y":true,"x":null}]},"alpha":"s","mid":-2.5}"#;
        assert_eq!(decode(text).to_string(), text);
    }

    /// Integers, negative numbers, and floats with a short form come
    /// back as written; a float written in a longer form is reduced
    /// to its shortest round-trip text.
    #[test]
    fn numbers_round_trip_in_their_shortest_form() {
        for text in [
            "0",
            "7",
            "-3",
            "1.5",
            "0.1",
            "12345678901234567890",
            "1e+21",
        ] {
            assert_eq!(decode(text).to_string(), text, "{text}");
        }
        assert_eq!(decode("1.50").to_string(), "1.5");
        assert_eq!(decode("1E3").to_string(), "1000.0");
    }

    /// Pointers resolve through objects and arrays with the RFC 6901
    /// escapes, `pointer_mut` edits in place, and `remove` takes a
    /// member out while its siblings keep their order.
    #[test]
    fn pointers_read_edit_and_remove() {
        let mut v = decode(r#"{"a/b":{"~":["x","y","z"]},"k":1,"m":2}"#);
        assert_eq!(v.pointer("/a~1b/~0/1").unwrap().as_str(), Some("y"));
        assert_eq!(v.pointer("").unwrap(), &v);
        assert!(v.pointer("/a~1b/~0/01").is_none());
        assert!(v.pointer("/nope").is_none());
        assert!(v.pointer("a").is_none());

        *v.pointer_mut("/a~1b/~0/1").unwrap() = EntryValue::String("Y".into());
        assert_eq!(v.remove("/k").unwrap(), EntryValue::Number(1.into()));
        assert_eq!(v.remove("/a~1b/~0/0").unwrap().as_str(), Some("x"));
        assert!(v.remove("").is_none());
        assert!(v.remove("/zzz").is_none());
        assert_eq!(v.to_string(), r#"{"a/b":{"~":["Y","z"]},"m":2}"#);
    }

    /// The string walk yields every string in document order with its
    /// pointer, escaped.
    #[test]
    fn strings_walk_in_document_order() {
        let v = decode(r#"{"z":"1","a":{"p/q":"2","n":3,"arr":["4",{"~k":"5"}]},"b":"6"}"#);
        let got: Vec<(String, &str)> = v.strings();
        assert_eq!(
            got,
            [
                ("/z".to_string(), "1"),
                ("/a/p~1q".to_string(), "2"),
                ("/a/arr/0".to_string(), "4"),
                ("/a/arr/1/~0k".to_string(), "5"),
                ("/b".to_string(), "6"),
            ]
        );
    }

    /// A `serde_json::Value` converts both ways; the way in takes the
    /// map's order, which is sorted.
    #[test]
    fn converts_to_and_from_serde_json_value() {
        let v = EntryValue::from(json!({"z": 1, "a": [true, null, "s"]}));
        assert_eq!(v.to_string(), r#"{"a":[true,null,"s"],"z":1}"#);
        let back = Value::from(v);
        assert_eq!(back, json!({"z": 1, "a": [true, null, "s"]}));
    }

    /// A number carried as `serde_json`'s private token, the shape
    /// `arbitrary_precision` sends through `deserialize_any`, decodes
    /// as a number.
    #[test]
    fn the_private_number_token_decodes_as_a_number() {
        let v = decode(&format!(r#"{{"{NUMBER_TOKEN}":"42"}}"#));
        assert_eq!(v, EntryValue::Number(42.into()));
    }

    /// An invalid entry is the fixed three-key shape, and only an
    /// entry of that type yields its raw text.
    #[test]
    fn an_invalid_entry_carries_its_raw_text() {
        let v = EntryValue::invalid_entry("expected `,`", r#"{"type":"user","a{"b":1}"#);
        assert_eq!(
            v.to_string(),
            r#"{"type":"$gage-invalid-entry","error":"expected `,`","raw":"{\"type\":\"user\",\"a{\"b\":1}"}"#
        );
        assert_eq!(v.invalid_entry_raw(), Some(r#"{"type":"user","a{"b":1}"#));
        assert_eq!(
            decode(r#"{"type":"user","raw":"x"}"#).invalid_entry_raw(),
            None
        );
        assert_eq!(
            decode(r#"{"type":"$gage-invalid-entry"}"#).invalid_entry_raw(),
            None
        );
    }
}
