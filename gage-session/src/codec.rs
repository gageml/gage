//! The entry codec: a session entry decoded from its native bytes into
//! an [`EntryValue`], and encoded back. An `EntryValue` is JSON whose
//! object keys keep the order the native content carries them, which
//! `serde_json::Value` does not, so an entry that is decoded, edited at
//! one value, and encoded again differs from the original only at
//! that value. A number keeps the text it was written as, which
//! `serde_json::Number` does not: `0.000005`, `1E3`, and `-0` come
//! back as written, where a `Number` writes `5e-6`, `1000.0`, and
//! `-0.0`.

use std::fmt;
use std::str::FromStr;

use indexmap::IndexMap;
use serde::de::{self, DeserializeOwned};
use serde::ser::{self, SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::{Number, Value};

use crate::DriverError;
use crate::text_block::TextDescriptor;

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
    Number(EntryNumber),
    String(String),
    Array(Vec<EntryValue>),
    Object(IndexMap<String, EntryValue>),
}

/// A JSON number as the text it was written with. The text is one
/// `serde_json` reads as a number, so it has a value; two numbers are
/// equal when their text is, and `1.0` is not `1.00`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryNumber(Box<str>);

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
            EntryValue::Number(n) => n.to_number().as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            EntryValue::Number(n) => n.to_number().as_u64(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            EntryValue::Number(n) => n.to_number().as_f64(),
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

impl EntryNumber {
    /// The number as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The number's value. An integer written with a fraction or an
    /// exponent, and `-0`, are floats, as `serde_json` reads them.
    pub fn to_number(&self) -> Number {
        self.0.parse().expect("number text should be a JSON number")
    }
}

impl fmt::Display for EntryNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Number> for EntryNumber {
    /// A number with no text of its own takes the text `serde_json`
    /// writes it as.
    fn from(number: Number) -> Self {
        EntryNumber(number.to_string().into())
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
    /// order to keep. A number takes the text `serde_json` writes.
    fn from(value: Value) -> Self {
        match value {
            Value::Null => EntryValue::Null,
            Value::Bool(b) => EntryValue::Bool(b),
            Value::Number(n) => EntryValue::Number(n.into()),
            Value::String(s) => EntryValue::String(s),
            Value::Array(items) => EntryValue::Array(items.into_iter().map(Into::into).collect()),
            Value::Object(map) => {
                EntryValue::Object(map.into_iter().map(|(k, v)| (k, v.into())).collect())
            }
        }
    }
}

impl From<EntryValue> for Value {
    /// An entry value as a `serde_json::Value`. Strings move; the
    /// containers are rebuilt, and an object's keys take the map's
    /// order, which is sorted without `preserve_order`. A number
    /// becomes its value and loses the text it was written as.
    fn from(value: EntryValue) -> Self {
        match value {
            EntryValue::Null => Value::Null,
            EntryValue::Bool(b) => Value::Bool(b),
            EntryValue::Number(n) => Value::Number(n.to_number()),
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

impl Serialize for EntryNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // A `RawValue` is the one form a serializer writes as the
        // text it holds.
        let raw: &RawValue = serde_json::from_str(&self.0).map_err(ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl FromStr for EntryValue {
    type Err = serde_json::Error;

    /// The one JSON value `text` holds, each number with its text.
    /// Text that is not JSON is the error `serde_json` gives for it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if let Some(value) = Parser::new(text).document() {
            return Ok(value);
        }
        serde_json::from_str::<Value>(text)?;
        Err(de::Error::custom(
            "the entry parser does not read this JSON",
        ))
    }
}

/// The depth of arrays and objects `serde_json` reads; one deeper is
/// its recursion limit.
const MAX_DEPTH: usize = 127;

/// Reads the structure of JSON text and has `serde_json` parse each
/// string and number where it stands, which also says where the token
/// ends, so a number's text is at hand. `serde_json` cannot read a
/// whole entry and keep that text: its deserializer yields a number
/// as a value, and its `arbitrary_precision` feature, which carries
/// the digits, still rewrites `1E3` as `1e+3` and `-0` as `0`.
///
/// Every method is `None` when the text at the position is not what
/// the method reads. The parser accepts the texts `serde_json` does,
/// so the caller gets the error from `serde_json`.
struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn document(mut self) -> Option<EntryValue> {
        self.skip_whitespace();
        let value = self.value(0)?;
        self.skip_whitespace();
        (self.pos == self.text.len()).then_some(value)
    }

    /// The value at the position, inside `depth` arrays and objects.
    fn value(&mut self, depth: usize) -> Option<EntryValue> {
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(EntryValue::String),
            b't' => self.literal("true", EntryValue::Bool(true)),
            b'f' => self.literal("false", EntryValue::Bool(false)),
            b'n' => self.literal("null", EntryValue::Null),
            _ => self.number(),
        }
    }

    /// An object. A key written twice keeps its first place and its
    /// last value.
    fn object(&mut self, depth: usize) -> Option<EntryValue> {
        if depth == MAX_DEPTH {
            return None;
        }
        self.pos += 1;
        let mut map = IndexMap::new();
        self.skip_whitespace();
        if self.eat(b'}') {
            return Some(EntryValue::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            if !self.eat(b':') {
                return None;
            }
            self.skip_whitespace();
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_whitespace();
            if self.eat(b'}') {
                return Some(EntryValue::Object(map));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<EntryValue> {
        if depth == MAX_DEPTH {
            return None;
        }
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.eat(b']') {
            return Some(EntryValue::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            if self.eat(b']') {
                return Some(EntryValue::Array(items));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.token().map(|(string, _)| string)
    }

    fn literal(&mut self, word: &str, value: EntryValue) -> Option<EntryValue> {
        let rest = self.text.get(self.pos..)?;
        rest.starts_with(word).then(|| {
            self.pos += word.len();
            value
        })
    }

    fn number(&mut self) -> Option<EntryValue> {
        let (_, text) = self.token::<Number>()?;
        Some(EntryValue::Number(EntryNumber(text.into())))
    }

    /// The token at the position as `serde_json` parses it, with its
    /// text. The position moves past the token.
    fn token<T: DeserializeOwned>(&mut self) -> Option<(T, &'a str)> {
        let rest = self.text.get(self.pos..)?;
        let mut tokens = serde_json::Deserializer::from_str(rest).into_iter::<T>();
        let token = tokens.next()?.ok()?;
        let text = rest.get(..tokens.byte_offset())?;
        self.pos += text.len();
        Some((token, text))
    }

    fn skip_whitespace(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    /// Step over `byte` when it is next.
    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.pos += 1;
        }
        found
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn decode(text: &str) -> EntryValue {
        text.parse().unwrap()
    }

    /// Keys come out in the order they went in, at every depth, and
    /// the compact text of an unedited value is the input.
    #[test]
    fn a_round_trip_keeps_document_order() {
        let text = r#"{"zeta":{"b":1,"a":[{"y":true,"x":null}]},"alpha":"s","mid":-2.5}"#;
        assert_eq!(decode(text).to_string(), text);
    }

    /// A number comes back as the text it was written with, in
    /// whatever form: JavaScript writes `0.000005` and
    /// `100000000000000000000` where the shortest form has an
    /// exponent, and a harness may write trailing zeros, a capital
    /// `E`, or a negative zero.
    #[test]
    fn a_number_keeps_the_text_it_was_written_with() {
        for text in [
            "0",
            "7",
            "-3",
            "1.5",
            "0.1",
            "12345678901234567890",
            "1e+21",
            "0.000005",
            "0.0000099",
            "1e-7",
            "100000000000000000000",
            "123456789012345678901234567890",
            "1.50",
            "1E3",
            "1e3",
            "-0",
            "-0.0",
            "1.0",
            "14.622697958000005",
            "9129.245015172533",
        ] {
            assert_eq!(decode(text).to_string(), text, "{text}");
        }
        let entry =
            r#"{"cost":0.000005,"big":100000000000000000000,"list":[1.50,1E3,-0,-0.0,1.0,1e-7]}"#;
        assert_eq!(decode(entry).to_string(), entry);
    }

    /// A number reads as the value `serde_json` gives its text: an
    /// integer in range is an integer, and a number written with a
    /// fraction or an exponent is a float only.
    #[test]
    fn a_number_reads_as_its_value() {
        assert_eq!(decode("7").as_i64(), Some(7));
        assert_eq!(decode("7").as_u64(), Some(7));
        assert_eq!(decode("7").as_f64(), Some(7.0));
        assert_eq!(decode("-3").as_i64(), Some(-3));
        assert_eq!(decode("-3").as_u64(), None);
        assert_eq!(decode("12345678901234567890").as_i64(), None);
        assert_eq!(
            decode("12345678901234567890").as_u64(),
            Some(12345678901234567890)
        );
        assert_eq!(decode("100000000000000000000").as_u64(), None);
        assert_eq!(decode("100000000000000000000").as_f64(), Some(1e20));
        assert_eq!(decode("0.000005").as_f64(), Some(0.000005));
        assert_eq!(decode("1.50").as_f64(), Some(1.5));
        assert_eq!(decode("1E3").as_f64(), Some(1000.0));
        assert_eq!(decode("1E3").as_i64(), None);
        assert_eq!(decode("1.0").as_i64(), None);
        assert_eq!(decode("-0").as_i64(), None);
        assert!(decode("-0").as_f64().unwrap().is_sign_negative());
        assert_eq!(decode("\"7\"").as_i64(), None);
    }

    /// Numbers are equal when their text is, so an edit that rewrites
    /// a number in another form is a change.
    #[test]
    fn numbers_are_equal_when_their_text_is() {
        assert_eq!(decode("1.50"), decode("1.50"));
        assert_ne!(decode("1.50"), decode("1.5"));
        assert_ne!(decode("1"), decode("1.0"));
    }

    /// A `serde_json::Value` holds a number's value and not its text:
    /// the way out drops the text, and the way in takes the text
    /// `serde_json` writes.
    #[test]
    fn a_number_as_a_json_value_is_in_its_shortest_form() {
        let v = decode("[0.000005,100000000000000000000,1.50,1E3,-0,7]");
        assert_eq!(
            v.to_json_value().to_string(),
            "[5e-6,1e+20,1.5,1000.0,-0.0,7]"
        );
        let back = EntryValue::from(json!([5e-6, 1.5, 7]));
        assert_eq!(back.to_string(), "[5e-6,1.5,7]");
    }

    /// A float of 16 or more digits has the same digits as a
    /// `serde_json::Value`: its text parses to the nearest double.
    /// Both numbers are from Claude Code sessions.
    #[test]
    fn a_long_float_keeps_its_digits_as_a_json_value() {
        for text in ["14.622697958000005", "9129.245015172533"] {
            assert_eq!(decode(text).to_json_value().to_string(), text, "{text}");
        }
    }

    /// A value serialized into a `serde_json::Value` is the value its
    /// conversion gives.
    #[test]
    fn a_number_serializes_into_a_json_value() {
        let v = decode(r#"{"a":1.50,"b":[1E3,7]}"#);
        assert_eq!(serde_json::to_value(&v).unwrap(), v.to_json_value());
    }

    /// Whitespace between tokens is read and not kept, and a key
    /// written twice keeps its first place and its last value.
    #[test]
    fn whitespace_and_repeated_keys_do_not_come_back() {
        let v = decode(" { \"a\" : [ 1 ,\t2.50 ] ,\r\n\"b\" : { } , \"c\" : [ ] } ");
        assert_eq!(v.to_string(), r#"{"a":[1,2.50],"b":{},"c":[]}"#);
        let v = decode(r#"{"a":1,"b":2,"a":3}"#);
        assert_eq!(v.to_string(), r#"{"a":3,"b":2}"#);
    }

    /// The parser reads every text `serde_json` reads, to the same
    /// value: each kind of token, escapes and text outside ASCII in
    /// strings and keys, and nesting up to the depth `serde_json`
    /// allows.
    #[test]
    fn text_serde_json_reads_decodes_to_the_same_value() {
        let deepest = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        let deepest_objects = r#"{"a":"#.repeat(MAX_DEPTH) + "1" + &"}".repeat(MAX_DEPTH);
        for text in [
            "null",
            "true",
            "false",
            " 1 ",
            "\"\"",
            "[]",
            "{}",
            "[null,true,false]",
            r#""a\"b\\c\/d\b\f\n\r\té😀""#,
            r#"{"k\"\né":"é😀","":[[],{}]}"#,
            "[0e999,1e-999,-1.5E+2,0.0]",
            r#"{"a":{"b":[{"c":null}]}}"#,
            deepest.as_str(),
            deepest_objects.as_str(),
        ] {
            let expected: Value = serde_json::from_str(text).unwrap();
            assert_eq!(decode(text).to_json_value(), expected, "{text}");
        }
    }

    /// A string comes back as JavaScript writes it: the short
    /// escapes, `\u` escapes for the other control characters, and
    /// every other character as itself, DEL, the line separators,
    /// and characters outside the basic plane among them. An escape
    /// a writer need not use decodes to its character.
    #[test]
    fn a_string_keeps_its_text() {
        for text in [
            r#""line\nbreak \"quoted\" back\\slash\ttab\r\b\f""#,
            r#""\u0000\u0001\u001f""#,
            "\"\u{7f}\u{a0}\u{2028}\u{2029}\u{feff}\"",
            "\"é 日本語 😀 𝒳\"",
            r#"{"k\n\"é😀":"<>&'/"}"#,
            "\"\"",
        ] {
            assert_eq!(decode(text).to_string(), text, "{text}");
        }
        assert_eq!(
            decode(r#""\u007f\/é😀\u000a""#).to_string(),
            "\"\u{7f}/é😀\\n\""
        );
    }

    /// A line of megabytes decodes and comes back as written.
    #[test]
    fn a_long_line_round_trips() {
        let chunk = r#"{"text":"some \"quoted\" text\n with é and 😀","n":[1.50,2,3E1]},"#;
        let mut text = String::from(r#"{"type":"user","content":["#);
        while text.len() < 4_000_000 {
            text.push_str(chunk);
        }
        text.push_str(r#"0.000005]}"#);
        assert_eq!(decode(&text).to_string(), text);
    }

    /// Nesting far past the limit is an error and not a stack
    /// overflow, in JSON that closes and in JSON broken after the
    /// nested part.
    #[test]
    fn very_deep_nesting_is_an_error() {
        let depth = 1_000_000;
        let closed = "[".repeat(depth) + &"]".repeat(depth);
        let broken = format!("[{closed},x]");
        let objects = r#"{"a":"#.repeat(depth) + "1" + &"}".repeat(depth);
        for text in [closed, broken, objects] {
            let err = text.parse::<EntryValue>().unwrap_err();
            assert!(err.is_syntax(), "{err}");
        }
    }

    /// Text that is not one JSON value is the error `serde_json`
    /// gives for it: a number it does not read, a string it does not
    /// read, a broken structure, and nesting past its limit.
    #[test]
    fn text_that_is_not_json_is_the_error_serde_json_gives() {
        let too_deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        let too_deep_objects = r#"{"a":"#.repeat(MAX_DEPTH + 1) + "1" + &"}".repeat(MAX_DEPTH + 1);
        for text in [
            "",
            " ",
            "nul",
            "truex",
            "[tru]",
            "1x",
            "01",
            "[01]",
            "[1.]",
            "[.5]",
            "[+1]",
            "[-]",
            "[1e]",
            "[1e999]",
            "[NaN]",
            "[1,]",
            "[,1]",
            "[1 2]",
            "[1",
            "[1]]",
            "{",
            r#"{"a"}"#,
            r#"{"a":}"#,
            r#"{"a":1,}"#,
            r#"{"a":1:2}"#,
            r#"{a:1}"#,
            r#"{1:1}"#,
            r#"{"a":1}{"b":2}"#,
            r#"{"a":"\ud83d"}"#,
            r#"{"a":"\ud800x"}"#,
            r#"{"a":"\udc00"}"#,
            r#"{"a":"\ud800\ud800"}"#,
            r#"{"a":"\ud83d\u0041"}"#,
            r#"{"a":"\x"}"#,
            r#"{"a":"\u12"}"#,
            "{\"a\":\"\u{0}\"}",
            "\u{feff}{}",
            "{}\u{a0}",
            "[true\"a\"]",
            "[\"a\"1]",
            r#"{"a":"b""c":1}"#,
            r#"{"type":"user","message":{"content":"Exit co{"type":"last-prompt"}"#,
            "\"open",
            "\"a\tb\"",
            too_deep.as_str(),
            too_deep_objects.as_str(),
        ] {
            let expected = serde_json::from_str::<Value>(text).unwrap_err();
            let got = text.parse::<EntryValue>().unwrap_err();
            assert_eq!(got.to_string(), expected.to_string(), "{text:?}");
            assert_eq!(got.classify(), expected.classify(), "{text:?}");
        }
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
        assert_eq!(v.remove("/k").unwrap(), decode("1"));
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

    /// A number is not a string: the string walk yields the strings
    /// of an entry and nothing for its numbers, in whatever form they
    /// are written, so keeping a number's text adds nothing to the
    /// text an entry is searched or redacted by.
    #[test]
    fn the_string_walk_leaves_numbers_out() {
        let v = decode(concat!(
            r#"{"cost":0.000005,"name":"n","scale":1E3,"zero":-0,"#,
            r#""big":12345678901234567890,"list":[1.50,"s",7,{"k":"t","m":1e-7}],"#,
            r#""digits":"0.000005"}"#
        ));
        assert_eq!(
            v.strings(),
            [
                ("/name".to_string(), "n"),
                ("/list/1".to_string(), "s"),
                ("/list/3/k".to_string(), "t"),
                ("/digits".to_string(), "0.000005"),
            ]
        );
        assert_eq!(v.pointer("/cost").unwrap().as_str(), None);
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
