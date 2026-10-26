//! The `text_block` table shape: one row per text value of a stored
//! session's entries, in document order within each entry. A driver
//! yields each entry's native JSON text and a [`TextDescriptor`] that
//! names the fields whose type is not text; the walk here emits the
//! rest. [`TextBlockBatch`] builds the table's batch, so every driver
//! produces one schema.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, LazyLock};

use datafusion::arrow::array::{Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::error::ArrowError;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use serde_json_path::JsonPath;

use crate::{DriverError, EntryJson};

/// One text value of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    /// 1-based line of the entry in the session's native content
    pub line: u32,
    /// The value's position among the entry's text values in document
    /// order, from 0
    pub seq: u32,
    pub text: String,
}

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("seq", DataType::Int64, false),
        Field::new("text", DataType::Utf8, false),
    ]))
});

pub fn text_block_schema() -> SchemaRef {
    SCHEMA.clone()
}

/// Accumulates [`TextBlock`] rows into the table's batch.
pub struct TextBlockBatch {
    session_ids: StringBuilder,
    lines: Int64Builder,
    seqs: Int64Builder,
    texts: StringBuilder,
}

impl Default for TextBlockBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBlockBatch {
    pub fn new() -> Self {
        Self {
            session_ids: StringBuilder::new(),
            lines: Int64Builder::new(),
            seqs: Int64Builder::new(),
            texts: StringBuilder::new(),
        }
    }

    /// Append one row under `session_id`.
    pub fn push(&mut self, session_id: &str, t: &TextBlock) {
        self.session_ids.append_value(session_id);
        self.lines.append_value(i64::from(t.line));
        self.seqs.append_value(i64::from(t.seq));
        self.texts.append_value(&t.text);
    }

    pub fn finish(mut self) -> std::result::Result<RecordBatch, ArrowError> {
        RecordBatch::try_new(
            text_block_schema(),
            vec![
                Arc::new(self.session_ids.finish()),
                Arc::new(self.lines.finish()),
                Arc::new(self.seqs.finish()),
                Arc::new(self.texts.finish()),
            ],
        )
    }
}

/// The `text_block` batch of one session from its rows. `session_id`
/// is written to every row.
pub fn text_block_batch(
    session_id: &str,
    rows: impl IntoIterator<Item = std::result::Result<TextBlock, DriverError>>,
) -> Result<RecordBatch> {
    let mut batch = TextBlockBatch::new();
    for row in rows {
        let t = row.map_err(|e| DataFusionError::External(Box::new(e)))?;
        batch.push(session_id, &t);
    }
    Ok(batch.finish()?)
}

/// The `text_block` rows of one entry: its text values under
/// `descriptor`, numbered in document order.
pub fn text_blocks(
    descriptor: &TextDescriptor,
    entry: &EntryJson,
) -> std::result::Result<Vec<TextBlock>, DriverError> {
    let values = descriptor
        .text_values(&entry.json)
        .map_err(|e| DriverError::Other(format!("line {}: {e}", entry.line)))?;
    Ok(values
        .into_iter()
        .enumerate()
        .map(|(seq, v)| TextBlock {
            line: entry.line,
            seq: seq as u32,
            text: v.text,
        })
        .collect())
}

/// What a driver knows about the strings of one content format: the
/// fields whose type is not text, and per named filter the fields
/// whose type says their value is sensitive. All are lists of JSONPath
/// (RFC 9535) patterns. A pattern selects a node and covers every
/// string under it.
#[derive(Debug, Clone)]
pub struct TextDescriptor {
    safe: PathSet,
    filters: Vec<FilterPaths>,
}

/// A set of path patterns matched against a string's JSON Pointer by
/// prefix. A plain pattern, member names and `[*]` only, is held as a
/// normalized pointer prefix in a prefix-free ordered set, so one
/// predecessor lookup decides it in O(log n) comparisons. A pattern
/// JSONPath alone can express, a filter or a descendant, is evaluated
/// against the parsed entry.
#[derive(Debug, Clone, Default)]
struct PathSet {
    prefixes: BTreeSet<String>,
    irregular: Vec<JsonPath>,
}

impl PathSet {
    fn compile(patterns: &[&str]) -> std::result::Result<Self, DescriptorError> {
        let mut set = Self::default();
        for pattern in patterns {
            match plain_prefix(pattern) {
                Some(prefix) => set.insert(prefix),
                None => {
                    set.irregular
                        .push(JsonPath::parse(pattern).map_err(|e| DescriptorError {
                            pattern: pattern.to_string(),
                            message: e.to_string(),
                        })?)
                }
            }
        }
        Ok(set)
    }

    /// Insert `prefix`, keeping the set prefix-free: a prefix already
    /// covered by an ancestor is redundant, and descendants of a new
    /// prefix are removed.
    fn insert(&mut self, prefix: String) {
        if self.covers_normalized(&prefix) {
            return;
        }
        let below: Vec<String> = self
            .prefixes
            .range(prefix.clone()..)
            .take_while(|p| covered_by(p, &prefix))
            .cloned()
            .collect();
        for p in below {
            self.prefixes.remove(&p);
        }
        self.prefixes.insert(prefix);
    }

    /// Whether a normalized pointer is at or under one of the plain
    /// prefixes. The set is prefix-free, so the greatest prefix at or
    /// below `key` is the only candidate.
    fn covers_normalized(&self, key: &str) -> bool {
        self.prefixes
            .range(..=key.to_string())
            .next_back()
            .is_some_and(|p| covered_by(key, p))
    }

    /// The pointers the irregular patterns select in `value`; empty
    /// when there are none.
    fn irregular_pointers(&self, value: Option<&Value>) -> Vec<String> {
        let Some(value) = value else {
            return Vec::new();
        };
        self.irregular
            .iter()
            .flat_map(|p| {
                p.query_located(value)
                    .locations()
                    .map(|l| l.to_json_pointer())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Whether a string at `pointer` (and its normalized form `key`)
    /// is covered, by a plain prefix or by a node an irregular pattern
    /// selected.
    fn covers(&self, pointer: &str, key: &str, irregular: &[String]) -> bool {
        self.covers_normalized(key) || irregular.iter().any(|root| covered_by(pointer, root))
    }

    fn has_irregular(&self) -> bool {
        !self.irregular.is_empty()
    }
}

/// Compile JSONPath patterns, naming the one that fails.
fn compile(patterns: &[&str]) -> std::result::Result<Vec<JsonPath>, DescriptorError> {
    patterns.iter().map(|p| compile_one(p)).collect()
}

fn compile_one(pattern: &str) -> std::result::Result<JsonPath, DescriptorError> {
    JsonPath::parse(pattern).map_err(|e| DescriptorError {
        pattern: pattern.to_string(),
        message: e.to_string(),
    })
}

/// Whether `key` equals `prefix` or continues it at a segment boundary.
fn covered_by(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The normalized pointer prefix of a plain pattern: `$`, then `.name`
/// and `[*]` steps, with a trailing `.*` or `[*]` selecting the parent
/// subtree. `None` for any other JSONPath, which is evaluated as such.
fn plain_prefix(pattern: &str) -> Option<String> {
    let mut rest = pattern.strip_prefix('$')?;
    let mut prefix = String::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("[*]") {
            prefix.push_str("/*");
            rest = after;
        } else if let Some(after) = rest.strip_prefix('.') {
            if after.starts_with('.') {
                return None;
            }
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let name = &after[..end];
            if name.is_empty() || name.contains('?') {
                return None;
            }
            if name == "*" {
                return if after[end..].is_empty() {
                    Some(prefix)
                } else {
                    None
                };
            }
            prefix.push('/');
            prefix.push_str(&name.replace('~', "~0").replace('/', "~1"));
            rest = &after[end..];
        } else {
            return None;
        }
    }
    Some(prefix)
}

/// A pointer with every array index replaced by `*`, the form the
/// plain prefixes take.
fn normalize(pointer: &str) -> String {
    pointer
        .split('/')
        .map(|seg| {
            if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
                "*"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The typed-sensitive paths of one named filter.
#[derive(Debug, Clone)]
struct FilterPaths {
    name: String,
    /// Fields whose value is itself the sensitive literal
    literal: Vec<JsonPath>,
    /// Fields whose whole value is replaced, each with its pattern text
    value: Vec<(String, JsonPath)>,
}

/// What a filter's typed-sensitive paths select in one entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterValues {
    /// The string values at the filter's `literal` paths
    pub literals: Vec<String>,
    /// The string values at the filter's `value` paths
    pub values: Vec<ValueHit>,
}

/// One string value a filter's `value` path selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueHit {
    /// The pattern that selected it
    pub path: String,
    /// JSON Pointer (RFC 6901) of the value from the entry root
    pub pointer: String,
}

/// A text value of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextValue {
    /// JSON Pointer (RFC 6901) of the value from the entry root
    pub pointer: String,
    pub text: String,
}

/// A descriptor pattern that is not JSONPath.
#[derive(Debug)]
pub struct DescriptorError {
    pub pattern: String,
    pub message: String,
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "descriptor pattern {:?}: {}", self.pattern, self.message)
    }
}

impl Error for DescriptorError {}

impl TextDescriptor {
    /// Compile the `safe` pattern list.
    pub fn new(safe: &[&str]) -> std::result::Result<Self, DescriptorError> {
        Ok(Self {
            safe: PathSet::compile(safe)?,
            filters: Vec::new(),
        })
    }

    /// Add the typed-sensitive paths of the named filter `name`.
    pub fn with_filter(
        mut self,
        name: &str,
        literal: &[&str],
        value: &[&str],
    ) -> std::result::Result<Self, DescriptorError> {
        self.filters.push(FilterPaths {
            name: name.to_string(),
            literal: compile(literal)?,
            value: value
                .iter()
                .map(|p| compile_one(p).map(|c| (p.to_string(), c)))
                .collect::<std::result::Result<_, _>>()?,
        });
        Ok(self)
    }

    /// The names of the filters with typed-sensitive paths.
    pub fn filter_names(&self) -> impl Iterator<Item = &str> {
        self.filters.iter().map(|f| f.name.as_str())
    }

    /// What the filter `name`'s typed-sensitive paths select in one
    /// entry. Empty for a filter without paths.
    pub fn filter_values(
        &self,
        name: &str,
        json: &str,
    ) -> std::result::Result<FilterValues, serde_json::Error> {
        let Some(paths) = self.filters.iter().find(|f| f.name == name) else {
            return Ok(FilterValues::default());
        };
        let entry: Value = serde_json::from_str(json)?;
        let literals = paths
            .literal
            .iter()
            .flat_map(|p| {
                p.query(&entry)
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .collect();
        let values = paths
            .value
            .iter()
            .flat_map(|(path, p)| {
                p.query_located(&entry)
                    .iter()
                    .filter(|n| n.node().is_string())
                    .map(|n| ValueHit {
                        path: path.clone(),
                        pointer: n.location().to_json_pointer(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(FilterValues { literals, values })
    }

    /// The text values of one entry, in document order. `json` is the
    /// entry as the harness wrote it. A string at or under a safe path
    /// is excluded. The entry is parsed into a `Value` only when an
    /// irregular pattern needs it.
    pub fn text_values(
        &self,
        json: &str,
    ) -> std::result::Result<Vec<TextValue>, serde_json::Error> {
        let value: Option<Value> = if self.safe.has_irregular() {
            Some(serde_json::from_str(json)?)
        } else {
            None
        };
        let safe_roots = self.safe.irregular_pointers(value.as_ref());
        let mut strings = Vec::new();
        let mut deserializer = serde_json::Deserializer::from_str(json);
        Collect {
            pointer: String::new(),
            out: &mut strings,
        }
        .deserialize(&mut deserializer)?;
        Ok(strings
            .into_iter()
            .filter_map(|(pointer, text)| {
                let key = normalize(&pointer);
                if self.safe.covers(&pointer, &key, &safe_roots) {
                    return None;
                }
                Some(TextValue { pointer, text })
            })
            .collect())
    }
}

/// Walks a JSON document in its written order and records every
/// string with its JSON Pointer. `serde_json::Value` sorts object
/// keys, so the walk runs on the deserializer, whose map visitor
/// receives keys in input order.
struct Collect<'a> {
    pointer: String,
    out: &'a mut Vec<(String, String)>,
}

impl<'de> DeserializeSeed<'de> for Collect<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Collect<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<(), E> {
        self.out.push((self.pointer, v.to_string()));
        Ok(())
    }

    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<(), E> {
        self.out.push((self.pointer, v));
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let out = self.out;
        while let Some(key) = map.next_key::<String>()? {
            map.next_value_seed(Collect {
                pointer: format!("{}/{}", self.pointer, escape(&key)),
                out: &mut *out,
            })?;
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        let out = self.out;
        let mut index = 0usize;
        while seq
            .next_element_seed(Collect {
                pointer: format!("{}/{index}", self.pointer),
                out: &mut *out,
            })?
            .is_some()
        {
            index += 1;
        }
        Ok(())
    }
}

/// A member name as a JSON Pointer reference token.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::{Array, Int64Array, StringArray};

    fn descriptor() -> TextDescriptor {
        TextDescriptor::new(&[
            "$.uuid",
            "$.type",
            "$.message.content[*].type",
            "$.message.content[*].id",
        ])
        .unwrap()
    }

    /// Strings come out in the order the line was written, not key
    /// order, with safe fields excluded.
    #[test]
    fn text_values_follow_document_order_and_the_descriptor() {
        let line = r#"{"zeta":"last key first","uuid":"u1","type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"toolUseResult":{"stdout":"out"},"alpha":"first key last"}"#;
        let values = descriptor().text_values(line).unwrap();
        let summary: Vec<(&str, &str)> = values
            .iter()
            .map(|v| (v.pointer.as_str(), v.text.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("/zeta", "last key first"),
                ("/message/content/0/text", "hi"),
                ("/message/content/1/name", "Bash"),
                ("/message/content/1/input/command", "ls"),
                ("/toolUseResult/stdout", "out"),
                ("/alpha", "first key last"),
            ]
        );
    }

    /// A member name with `/` or `~` is escaped in the pointer, and a
    /// string that resembles an identifier is still text when its
    /// field is not on the safe list.
    #[test]
    fn pointers_escape_member_names() {
        let line = r#"{"a/b":"x","c~d":"y","note":"11111111-2222-3333-4444-555555555555"}"#;
        let values = descriptor().text_values(line).unwrap();
        let pointers: Vec<&str> = values.iter().map(|v| v.pointer.as_str()).collect();
        assert_eq!(pointers, ["/a~1b", "/c~0d", "/note"]);
    }

    #[test]
    fn filter_values_select_literals_and_whole_values() {
        let descriptor = descriptor()
            .with_filter("pii", &["$..user_identity"], &["$.context.email"])
            .unwrap();
        let line = r#"{"context":{"email":"The user is a@b.c"},"params":{"classifier":{"user_identity":"garrett"}},"n":{"user_identity":5}}"#;
        let found = descriptor.filter_values("pii", line).unwrap();
        assert_eq!(found.literals, ["garrett"]);
        assert_eq!(
            found.values,
            [ValueHit {
                path: "$.context.email".into(),
                pointer: "/context/email".into(),
            }]
        );
        assert_eq!(
            descriptor.filter_values("secret", line).unwrap(),
            FilterValues::default()
        );
        assert_eq!(descriptor.filter_names().collect::<Vec<_>>(), ["pii"]);
    }

    /// A safe pattern covers the subtree under the node it selects, and
    /// a descendant pattern falls back to JSONPath evaluation.
    #[test]
    fn safe_covers_subtrees_and_irregular_patterns_still_work() {
        let descriptor = TextDescriptor::new(&[
            "$.message.usage",
            "$..git_state.error",
            "$.permissionDecision.*",
        ])
        .unwrap();
        let line = r#"{"message":{"usage":{"service_tier":"standard","iterations":[{"type":"message"}]},"text":"hi"},"permissionDecision":{"decision":"accept"},"params":{"git_state":{"error":"pending","root":"/w"}}}"#;
        let pointers: Vec<String> = descriptor
            .text_values(line)
            .unwrap()
            .into_iter()
            .map(|v| v.pointer)
            .collect();
        assert_eq!(pointers, ["/message/text", "/params/git_state/root"]);
    }

    #[test]
    fn plain_prefixes_normalize_and_the_set_stays_prefix_free() {
        assert_eq!(
            plain_prefix("$.message.content[*].type").as_deref(),
            Some("/message/content/*/type")
        );
        assert_eq!(
            plain_prefix("$.permissionDecision.*").as_deref(),
            Some("/permissionDecision")
        );
        assert_eq!(plain_prefix("$..git_state").as_deref(), None);
        assert_eq!(
            plain_prefix("$.message.content[?@.type == 'x']").as_deref(),
            None
        );
        assert_eq!(
            normalize("/message/content/12/text"),
            "/message/content/*/text"
        );

        let set = PathSet::compile(&["$.a.b", "$.a", "$.a.c", "$.ab"]).unwrap();
        assert_eq!(
            set.prefixes.iter().cloned().collect::<Vec<_>>(),
            ["/a", "/ab"]
        );
        assert!(set.covers_normalized("/a/c/d"));
        assert!(!set.covers_normalized("/abc"));
    }

    #[test]
    fn a_bad_pattern_is_an_error_naming_it() {
        let err = TextDescriptor::new(&["$.ok", "$[?"]).unwrap_err();
        assert_eq!(err.pattern, "$[?");
    }

    #[test]
    fn text_blocks_number_the_values_of_an_entry() {
        let entry = EntryJson {
            line: 7,
            json: r#"{"uuid":"u","a":"one","b":{"c":"two"}}"#.to_string(),
        };
        let rows = text_blocks(&descriptor(), &entry).unwrap();
        assert_eq!(
            rows,
            [
                TextBlock {
                    line: 7,
                    seq: 0,
                    text: "one".to_string()
                },
                TextBlock {
                    line: 7,
                    seq: 1,
                    text: "two".to_string()
                },
            ]
        );
    }

    #[test]
    fn an_unparseable_entry_names_its_line() {
        let entry = EntryJson {
            line: 3,
            json: "{not json".to_string(),
        };
        let err = text_blocks(&descriptor(), &entry).unwrap_err();
        assert!(err.to_string().starts_with("line 3:"), "{err}");
    }

    #[test]
    fn batch_matches_the_schema() {
        let rows = [
            Ok(TextBlock {
                line: 1,
                seq: 0,
                text: "a".to_string(),
            }),
            Ok(TextBlock {
                line: 1,
                seq: 1,
                text: "b".to_string(),
            }),
        ];
        let batch = text_block_batch("s1", rows).unwrap();
        assert_eq!(batch.schema(), text_block_schema());
        assert_eq!(batch.num_rows(), 2);
        let seqs = batch
            .column_by_name("seq")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(seqs.values(), &[0, 1]);
        let texts = batch
            .column_by_name("text")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(texts.value(1), "b");
        assert_eq!(texts.len(), 2);
    }

    #[test]
    fn a_driver_error_fails_the_batch() {
        let err = text_block_batch("s1", [Err(DriverError::Other("bad line".into()))])
            .err()
            .unwrap();
        assert!(err.to_string().contains("bad line"), "{err}");
    }
}
