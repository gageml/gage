//! The `text_block` table shape: one row per text value of a stored
//! session's entries, in document order within each entry. A driver
//! yields each entry's native JSON text and a [`TextDescriptor`] that
//! names the fields whose type is not text; the walk here emits the
//! rest. [`TextBlockBatch`] builds the table's batch, so every driver
//! produces one schema.

use std::collections::HashSet;
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
/// fields whose type is not text, and the subtrees whose text values
/// are redacted whole. Both are lists of JSONPath (RFC 9535) patterns
/// evaluated against each entry.
#[derive(Debug, Clone)]
pub struct TextDescriptor {
    safe: Vec<JsonPath>,
    atomic: Vec<JsonPath>,
}

/// A text value of one entry with its classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextValue {
    /// JSON Pointer (RFC 6901) of the value from the entry root
    pub pointer: String,
    pub text: String,
    /// Within an atomic subtree, so redacted whole on any match
    pub atomic: bool,
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
    /// Compile the `safe` and `atomic` pattern lists.
    pub fn new(safe: &[&str], atomic: &[&str]) -> std::result::Result<Self, DescriptorError> {
        Ok(Self {
            safe: compile(safe)?,
            atomic: compile(atomic)?,
        })
    }

    /// The text values of one entry, in document order. `json` is the
    /// entry as the harness wrote it. A string at a safe path is
    /// excluded even inside an atomic subtree.
    pub fn text_values(
        &self,
        json: &str,
    ) -> std::result::Result<Vec<TextValue>, serde_json::Error> {
        let value: Value = serde_json::from_str(json)?;
        let safe = pointers(&self.safe, &value);
        let atomic = pointers(&self.atomic, &value);
        let mut strings = Vec::new();
        let mut deserializer = serde_json::Deserializer::from_str(json);
        Collect {
            pointer: String::new(),
            out: &mut strings,
        }
        .deserialize(&mut deserializer)?;
        Ok(strings
            .into_iter()
            .filter(|(pointer, _)| !safe.contains(pointer))
            .map(|(pointer, text)| {
                let atomic = atomic
                    .iter()
                    .any(|root| pointer == *root || pointer.starts_with(&format!("{root}/")));
                TextValue {
                    pointer,
                    text,
                    atomic,
                }
            })
            .collect())
    }
}

fn compile(patterns: &[&str]) -> std::result::Result<Vec<JsonPath>, DescriptorError> {
    patterns
        .iter()
        .map(|p| {
            JsonPath::parse(p).map_err(|e| DescriptorError {
                pattern: p.to_string(),
                message: e.to_string(),
            })
        })
        .collect()
}

/// The JSON Pointers of every node the patterns select in `value`.
fn pointers(paths: &[JsonPath], value: &Value) -> HashSet<String> {
    paths
        .iter()
        .flat_map(|p| {
            p.query_located(value)
                .locations()
                .map(|l| l.to_json_pointer())
                .collect::<Vec<_>>()
        })
        .collect()
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
        TextDescriptor::new(
            &[
                "$.uuid",
                "$.type",
                "$.message.content[*].type",
                "$.message.content[*].id",
            ],
            &[
                "$.message.content[?@.type == 'tool_use']",
                "$.toolUseResult",
            ],
        )
        .unwrap()
    }

    /// Strings come out in the order the line was written, not key
    /// order, with safe fields excluded and atomic state inherited
    /// from the matched subtree root.
    #[test]
    fn text_values_follow_document_order_and_the_descriptor() {
        let line = r#"{"zeta":"last key first","uuid":"u1","type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"toolUseResult":{"stdout":"out"},"alpha":"first key last"}"#;
        let values = descriptor().text_values(line).unwrap();
        let summary: Vec<(&str, &str, bool)> = values
            .iter()
            .map(|v| (v.pointer.as_str(), v.text.as_str(), v.atomic))
            .collect();
        assert_eq!(
            summary,
            [
                ("/zeta", "last key first", false),
                ("/message/content/0/text", "hi", false),
                ("/message/content/1/name", "Bash", true),
                ("/message/content/1/input/command", "ls", true),
                ("/toolUseResult/stdout", "out", true),
                ("/alpha", "first key last", false),
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
    fn a_bad_pattern_is_an_error_naming_it() {
        let err = TextDescriptor::new(&["$.ok", "$[?"], &[]).unwrap_err();
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
