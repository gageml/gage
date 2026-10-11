//! The `Message` and `Entry` row values the session queries yield,
//! their readers over query batches, and the SQL fragments for a
//! `.type(spec)` filter and a line range.

use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use rune::alloc;
use rune::alloc::fmt::TryWrite;
use rune::alloc::prelude::TryClone;
use rune::runtime::{Formatter, Object, Protocol, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::datetime::DateTime;
use crate::error::Error;

/// Constrain `line` to the session's range when one is set.
pub fn push_lines_clause(
    lines: Option<(u64, u64)>,
    clauses: &mut Vec<String>,
    params: &mut Vec<ScalarValue>,
) {
    if let Some((start, end)) = lines {
        params.push(ScalarValue::Int64(Some(start as i64)));
        clauses.push(format!("line >= ${}", params.len()));
        params.push(ScalarValue::Int64(Some(end as i64)));
        clauses.push(format!("line <= ${}", params.len()));
    }
}

pub fn type_clause(
    spec: &serde_json::Value,
    params: &mut Vec<ScalarValue>,
) -> std::result::Result<String, Error> {
    use serde_json::Value as J;
    match spec {
        J::String(s) => {
            params.push(ScalarValue::Utf8(Some(s.clone())));
            Ok(format!("type = ${}", params.len()))
        }
        J::Array(items) => {
            let placeholders = string_in_list(items, params, "type")?;
            Ok(format!("type IN ({placeholders})"))
        }
        J::Object(map) => {
            if map.is_empty() {
                return Err(Error::Args(
                    "`.type()` object must name at least one type".into(),
                ));
            }
            let mut ors = Vec::with_capacity(map.len());
            for (ty, sub) in map {
                params.push(ScalarValue::Utf8(Some(ty.clone())));
                let type_eq = format!("type = ${}", params.len());
                let sub_clause = match sub {
                    J::String(s) => {
                        params.push(ScalarValue::Utf8(Some(s.clone())));
                        format!("subtype = ${}", params.len())
                    }
                    J::Array(items) => {
                        let placeholders = string_in_list(items, params, "subtype")?;
                        format!("subtype IN ({placeholders})")
                    }
                    _ => {
                        return Err(Error::Args(
                            "`.type()` subtype must be a string or array of strings".into(),
                        ));
                    }
                };
                ors.push(format!("({type_eq} AND {sub_clause})"));
            }
            Ok(format!("({})", ors.join(" OR ")))
        }
        _ => Err(Error::Args(
            "`.type()` expects a string, array of strings, or object".into(),
        )),
    }
}

pub(crate) fn string_in_list(
    items: &[serde_json::Value],
    params: &mut Vec<ScalarValue>,
    field: &str,
) -> Result<String, Error> {
    if items.is_empty() {
        return Err(Error::Args(format!(
            "`.type()` {field} list must be non-empty"
        )));
    }
    let mut placeholders = Vec::with_capacity(items.len());
    for item in items {
        let s = item.as_str().ok_or_else(|| {
            Error::Args(format!("`.type()` {field} list must contain only strings"))
        })?;
        params.push(ScalarValue::Utf8(Some(s.to_string())));
        placeholders.push(format!("${}", params.len()));
    }
    Ok(placeholders.join(", "))
}

/// Register the `Message` and `Entry` row values without the query
/// builders that produce them, for a runtime with builders of its
/// own.
pub fn register_row_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Message>()?;
    m.field_function(&Protocol::GET, "session_id", |m: &Message| {
        m.get("session_id")
    })?;
    m.field_function(&Protocol::GET, "line", |m: &Message| m.get("line"))?;
    m.field_function(&Protocol::GET, "uuid", |m: &Message| m.get("uuid"))?;
    m.field_function(&Protocol::GET, "type", |m: &Message| m.get("type"))?;
    m.field_function(&Protocol::GET, "subtype", |m: &Message| m.get("subtype"))?;
    m.field_function(&Protocol::GET, "text", |m: &Message| m.get("text"))?;
    m.field_function(&Protocol::GET, "timestamp", |m: &Message| {
        m.get("timestamp")
    })?;
    m.field_function(&Protocol::GET, "attachments", |m: &Message| {
        m.get("attachments")
    })?;
    m.field_function(&Protocol::GET, "ide_tags", |m: &Message| m.get("ide_tags"))?;
    m.field_function(&Protocol::GET, "raw", |m: &Message| m.get("raw"))?;
    m.function_meta(Message::as_object)?;
    m.function_meta(Message::model)?;
    m.function_meta(Message::to_json)?;
    m.function_meta(Message::debug)?;

    m.ty::<Entry>()?;
    m.field_function(&Protocol::GET, "session_id", |e: &Entry| {
        e.get("session_id")
    })?;
    m.field_function(&Protocol::GET, "line", |e: &Entry| e.get("line"))?;
    m.field_function(&Protocol::GET, "uuid", |e: &Entry| e.get("uuid"))?;
    m.field_function(&Protocol::GET, "type", |e: &Entry| e.get("type"))?;
    m.field_function(&Protocol::GET, "subtype", |e: &Entry| e.get("subtype"))?;
    m.field_function(&Protocol::GET, "timestamp", |e: &Entry| e.get("timestamp"))?;
    m.field_function(&Protocol::GET, "raw", |e: &Entry| e.get("raw"))?;
    m.function_meta(Entry::as_object)?;
    m.function_meta(Entry::to_json)?;
    m.function_meta(Entry::debug)?;
    Ok(())
}

fn key(s: &str) -> alloc::String {
    alloc::String::try_from(s).unwrap()
}

fn debug_fields(
    f: &mut Formatter,
    name: &str,
    fields: &[&str],
    get: impl Fn(&str) -> Value,
) -> Result<(), VmError> {
    write!(f, "{name} {{ ")?;
    for (i, attr) in fields.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{attr}: ")?;
        get(attr).debug_fmt(f)?;
    }
    write!(f, " }}")?;
    Ok(())
}

fn str_to_val(s: &str) -> Value {
    let rs = alloc::String::try_from(s).unwrap();
    Value::new(rs).unwrap()
}

fn optional_str_to_val(arr: &StringArray, row: usize) -> Value {
    if arr.is_null(row) {
        rune::to_value(Option::<rune::alloc::String>::None).unwrap()
    } else {
        rune::to_value(Some(alloc::String::try_from(arr.value(row)).unwrap())).unwrap()
    }
}

// A message always has a timestamp; a null column value (which the
// data never actually carries) falls back to the epoch.
fn ts_to_val(arr: &TimestampMillisecondArray, row: usize) -> Value {
    let ms = if arr.is_null(row) { 0 } else { arr.value(row) };
    rune::to_value(DateTime::from_millis(ms)).unwrap()
}

fn optional_ts_to_val(arr: &TimestampMillisecondArray, row: usize) -> Value {
    if arr.is_null(row) {
        rune::to_value(Option::<DateTime>::None).unwrap()
    } else {
        rune::to_value(Some(DateTime::from_millis(arr.value(row)))).unwrap()
    }
}

#[derive(Any)]
pub struct Message {
    pub inner: Object,
    #[rune(skip)]
    pub object: std::sync::OnceLock<Value>,
}

impl Message {
    fn get(&self, attr: &str) -> Value {
        let k = key(attr);
        self.inner
            .get(&k)
            .cloned()
            .unwrap_or_else(|| rune::to_value(()).unwrap())
    }

    fn object(&self) -> Value {
        self.object
            .get_or_init(|| {
                let raw_val = self.get("raw");
                let raw_str = raw_val.borrow_string_ref().unwrap();
                let val: serde_json::Value = serde_json::from_str(&raw_str).unwrap();
                rune::to_value(super::value::json_to_object(&val)).unwrap()
            })
            .clone()
    }

    #[rune::function]
    pub fn as_object(&self) -> Value {
        self.object()
    }

    #[rune::function]
    pub fn model(&self) -> Option<String> {
        let object = self.object();
        let object = object.borrow_ref::<Object>().unwrap();
        let message = object.get(&key("message"))?.clone();
        let message = message.borrow_ref::<Object>().unwrap();
        let model = message.get(&key("model"))?;
        Some(model.borrow_string_ref().unwrap().to_string())
    }

    #[rune::function]
    pub fn to_json(&self) -> Value {
        rune::to_value(self.inner.try_clone().unwrap()).unwrap()
    }

    // raw is omitted: it's the JSON source the other fields are decoded
    // from, so printing it would swamp the output
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        let fields = [
            "session_id",
            "line",
            "uuid",
            "type",
            "subtype",
            "text",
            "timestamp",
            "attachments",
            "ide_tags",
        ];
        debug_fields(f, "Message", &fields, |attr| self.get(attr))
    }
}

pub fn messages_from_batches(batches: Vec<RecordBatch>) -> Vec<Message> {
    let mut messages = Vec::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let schema = batch.schema();
        let col_session_id = schema.index_of("session_id").unwrap();
        let col_line = schema.index_of("line").unwrap();
        let col_uuid = schema.index_of("uuid").unwrap();
        let col_type = schema.index_of("type").unwrap();
        let col_subtype = schema.index_of("subtype").unwrap();
        let col_text = schema.index_of("text").unwrap();
        let col_timestamp = schema.index_of("timestamp").unwrap();
        let col_attachments = schema.index_of("attachments").unwrap();
        let col_ide_tags = schema.index_of("ide_tags").unwrap();
        let col_raw = schema.index_of("raw").unwrap();

        let session_id_arr = batch
            .column(col_session_id)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let line_arr = batch
            .column(col_line)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let uuid_arr = batch
            .column(col_uuid)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let type_arr = batch
            .column(col_type)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let subtype_arr = batch
            .column(col_subtype)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let text_arr = batch
            .column(col_text)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let timestamp_arr = batch
            .column(col_timestamp)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .unwrap();
        let attachments_arr = batch
            .column(col_attachments)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let ide_tags_arr = batch
            .column(col_ide_tags)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let raw_arr = batch
            .column(col_raw)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();

        for row in 0..batch.num_rows() {
            let mut inner = Object::new();
            inner
                .insert(key("session_id"), str_to_val(session_id_arr.value(row)))
                .unwrap();
            inner
                .insert(key("line"), rune::to_value(line_arr.value(row)).unwrap())
                .unwrap();
            inner
                .insert(key("uuid"), optional_str_to_val(uuid_arr, row))
                .unwrap();
            inner
                .insert(key("type"), str_to_val(type_arr.value(row)))
                .unwrap();
            inner
                .insert(key("subtype"), optional_str_to_val(subtype_arr, row))
                .unwrap();
            inner
                .insert(key("text"), str_to_val(text_arr.value(row)))
                .unwrap();
            inner
                .insert(key("timestamp"), ts_to_val(timestamp_arr, row))
                .unwrap();
            inner
                .insert(
                    key("attachments"),
                    optional_str_to_val(attachments_arr, row),
                )
                .unwrap();
            inner
                .insert(key("ide_tags"), optional_str_to_val(ide_tags_arr, row))
                .unwrap();
            inner
                .insert(key("raw"), str_to_val(raw_arr.value(row)))
                .unwrap();
            messages.push(Message {
                inner,
                object: std::sync::OnceLock::new(),
            });
        }
    }
    messages
}

#[derive(Any)]
pub struct Entry {
    pub inner: Object,
    #[rune(skip)]
    pub object: std::sync::OnceLock<Value>,
}

impl Entry {
    fn get(&self, attr: &str) -> Value {
        let k = key(attr);
        self.inner
            .get(&k)
            .cloned()
            .unwrap_or_else(|| rune::to_value(()).unwrap())
    }

    #[rune::function]
    pub fn as_object(&self) -> Value {
        self.object
            .get_or_init(|| {
                let raw_val = self.get("raw");
                let raw_str = raw_val.borrow_string_ref().unwrap();
                let val: serde_json::Value = serde_json::from_str(&raw_str).unwrap();
                rune::to_value(super::value::json_to_object(&val)).unwrap()
            })
            .clone()
    }

    #[rune::function]
    pub fn to_json(&self) -> Value {
        rune::to_value(self.inner.try_clone().unwrap()).unwrap()
    }

    // raw is omitted: it's the JSON source the other fields are decoded
    // from, so printing it would swamp the output
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        let fields = ["session_id", "line", "uuid", "type", "subtype", "timestamp"];
        debug_fields(f, "Entry", &fields, |attr| self.get(attr))
    }
}

pub fn entries_from_batches(batches: Vec<RecordBatch>) -> Vec<Entry> {
    let mut entries = Vec::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let schema = batch.schema();
        let col_session_id = schema.index_of("session_id").unwrap();
        let col_line = schema.index_of("line").unwrap();
        let col_uuid = schema.index_of("uuid").unwrap();
        let col_type = schema.index_of("type").unwrap();
        let col_subtype = schema.index_of("subtype").unwrap();
        let col_timestamp = schema.index_of("timestamp").unwrap();
        let col_raw = schema.index_of("raw").unwrap();

        let session_id_arr = batch
            .column(col_session_id)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let line_arr = batch
            .column(col_line)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        let uuid_arr = batch
            .column(col_uuid)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let type_arr = batch
            .column(col_type)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let subtype_arr = batch
            .column(col_subtype)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let timestamp_arr = batch
            .column(col_timestamp)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .unwrap();
        let raw_arr = batch
            .column(col_raw)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();

        for row in 0..batch.num_rows() {
            let mut inner = Object::new();
            inner
                .insert(key("session_id"), str_to_val(session_id_arr.value(row)))
                .unwrap();
            inner
                .insert(key("line"), rune::to_value(line_arr.value(row)).unwrap())
                .unwrap();
            inner
                .insert(key("uuid"), optional_str_to_val(uuid_arr, row))
                .unwrap();
            inner
                .insert(key("type"), str_to_val(type_arr.value(row)))
                .unwrap();
            inner
                .insert(key("subtype"), optional_str_to_val(subtype_arr, row))
                .unwrap();
            inner
                .insert(key("timestamp"), optional_ts_to_val(timestamp_arr, row))
                .unwrap();
            inner
                .insert(key("raw"), str_to_val(raw_arr.value(row)))
                .unwrap();
            entries.push(Entry {
                inner,
                object: std::sync::OnceLock::new(),
            });
        }
    }
    entries
}
