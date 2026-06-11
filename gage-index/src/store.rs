//! Columnar store: per-session Parquet files of derived rows plus a
//! consolidated `sessions.parquet` of session aggregates.
//!
//! Session files are self-describing — the source fingerprint lives
//! in the Parquet footer metadata, so the store needs no external
//! state tracking. All writes are temp-file-plus-atomic-rename:
//! readers never see partial files, and racing writers of the same
//! session are benign duplicates (derivation is deterministic).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, BooleanBuilder, Int64Array, Int64Builder, StringArray, StringBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::{KeyValue, ParquetMetaDataReader};
use parquet::file::properties::WriterProperties;

use crate::derive::{COL_LINE, COL_TEXT, DerivedSession, Fingerprint, SessionAggregates};
use crate::{IndexError, Result};

/// Store format version: covers the store schema and anything else
/// that affects session-file contents. Bumping it changes the `v{N}`
/// path component, so old artifacts are abandoned and rebuilt.
pub const STORE_FORMAT_VERSION: u32 = 1;

const FP_MTIME_KEY: &str = "gage:source_mtime_ms";
const FP_SIZE_KEY: &str = "gage:source_size";

fn writer_properties(fingerprint: Option<Fingerprint>) -> WriterProperties {
    let mut builder =
        WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::default()));
    if let Some(fp) = fingerprint {
        builder = builder.set_key_value_metadata(Some(vec![
            KeyValue::new(FP_MTIME_KEY.to_string(), fp.mtime_ms.to_string()),
            KeyValue::new(FP_SIZE_KEY.to_string(), fp.size.to_string()),
        ]));
    }
    builder.build()
}

fn write_parquet(
    path: &Path,
    schema: SchemaRef,
    batch: &RecordBatch,
    fingerprint: Option<Fingerprint>,
) -> Result<()> {
    let tmp = path.with_extension("parquet.tmp");
    let file = File::create(&tmp)?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(writer_properties(fingerprint)))?;
    writer.write(batch)?;
    writer.close()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write a derived session to `{session_id}.parquet` in `dir`.
pub(crate) fn write_session_file(dir: &Path, derived: &DerivedSession) -> Result<PathBuf> {
    let path = dir.join(format!("{}.parquet", derived.session_id));
    write_parquet(
        &path,
        derived.batch.schema(),
        &derived.batch,
        Some(derived.fingerprint),
    )?;
    Ok(path)
}

/// Read the source fingerprint from a session file's footer metadata.
/// `None` when the file has no fingerprint (foreign or corrupt file —
/// the caller treats it as dirty).
pub(crate) fn read_fingerprint(path: &Path) -> Option<Fingerprint> {
    let file = File::open(path).ok()?;
    let metadata = ParquetMetaDataReader::new().parse_and_finish(&file).ok()?;
    let kv = metadata.file_metadata().key_value_metadata()?;
    let mut mtime_ms = None;
    let mut size = None;
    for entry in kv {
        match entry.key.as_str() {
            FP_MTIME_KEY => mtime_ms = entry.value.as_deref().and_then(|v| v.parse().ok()),
            FP_SIZE_KEY => size = entry.value.as_deref().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    Some(Fingerprint {
        mtime_ms: mtime_ms?,
        size: size?,
    })
}

/// Read `(line, text)` for every message row of a session file —
/// the rows the text index covers. Used for index rebuilds that scan
/// the store instead of re-parsing JSONL.
pub(crate) fn read_message_rows(path: &Path) -> Result<Vec<(i64, String)>> {
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let mask =
        parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), [COL_LINE, COL_TEXT]);
    let reader = builder.with_projection(mask).build()?;
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch?;
        let lines = downcast::<Int64Array>(&batch, 0, "line")?;
        let texts = downcast::<StringArray>(&batch, 1, "text")?;
        for i in 0..batch.num_rows() {
            if texts.is_valid(i) {
                rows.push((lines.value(i), texts.value(i).to_string()));
            }
        }
    }
    Ok(rows)
}

fn downcast<'a, T: 'static>(batch: &'a RecordBatch, col: usize, name: &str) -> Result<&'a T> {
    batch
        .columns()
        .get(col)
        .and_then(|c| c.as_any().downcast_ref::<T>())
        .ok_or_else(|| {
            IndexError::Arrow(arrow::error::ArrowError::SchemaError(format!(
                "unexpected column type for {name}"
            )))
        })
}

fn aggregates_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("message_count", DataType::Int64, false),
        Field::new("input_tokens", DataType::Int64, false),
        Field::new("output_tokens", DataType::Int64, false),
        Field::new("cache_read_input_tokens", DataType::Int64, false),
        Field::new("cache_creation_input_tokens", DataType::Int64, false),
        Field::new("is_empty", DataType::Boolean, false),
    ]))
}

/// Rewrite the consolidated session-aggregates file.
pub(crate) fn write_aggregates(
    path: &Path,
    aggregates: &BTreeMap<String, SessionAggregates>,
) -> Result<()> {
    let mut ids = StringBuilder::new();
    let mut titles = StringBuilder::new();
    let mut models = StringBuilder::new();
    let mut message_counts = Int64Builder::new();
    let mut input_tokens = Int64Builder::new();
    let mut output_tokens = Int64Builder::new();
    let mut cache_read = Int64Builder::new();
    let mut cache_creation = Int64Builder::new();
    let mut is_empty = BooleanBuilder::new();

    for (id, a) in aggregates {
        ids.append_value(id);
        match &a.title {
            Some(t) => titles.append_value(t),
            None => titles.append_null(),
        }
        match &a.model {
            Some(m) => models.append_value(m),
            None => models.append_null(),
        }
        message_counts.append_value(a.message_count);
        input_tokens.append_value(a.input_tokens);
        output_tokens.append_value(a.output_tokens);
        cache_read.append_value(a.cache_read_input_tokens);
        cache_creation.append_value(a.cache_creation_input_tokens);
        is_empty.append_value(a.is_empty);
    }

    let schema = aggregates_schema();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(ids.finish()),
            Arc::new(titles.finish()),
            Arc::new(models.finish()),
            Arc::new(message_counts.finish()),
            Arc::new(input_tokens.finish()),
            Arc::new(output_tokens.finish()),
            Arc::new(cache_read.finish()),
            Arc::new(cache_creation.finish()),
            Arc::new(is_empty.finish()),
        ],
    )?;
    write_parquet(path, schema, &batch, None)
}

/// Load the consolidated session aggregates. An absent file is an
/// empty map (first run); sessions missing from the map are treated
/// as dirty by the reconciler.
pub(crate) fn read_aggregates(path: &Path) -> Result<HashMap<String, SessionAggregates>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut map = HashMap::new();
    for batch in reader {
        let batch = batch?;
        let ids = downcast::<StringArray>(&batch, 0, "id")?;
        let titles = downcast::<StringArray>(&batch, 1, "title")?;
        let models = downcast::<StringArray>(&batch, 2, "model")?;
        let message_counts = downcast::<Int64Array>(&batch, 3, "message_count")?;
        let input_tokens = downcast::<Int64Array>(&batch, 4, "input_tokens")?;
        let output_tokens = downcast::<Int64Array>(&batch, 5, "output_tokens")?;
        let cache_read = downcast::<Int64Array>(&batch, 6, "cache_read_input_tokens")?;
        let cache_creation = downcast::<Int64Array>(&batch, 7, "cache_creation_input_tokens")?;
        let is_empty = downcast::<BooleanArray>(&batch, 8, "is_empty")?;
        for i in 0..batch.num_rows() {
            map.insert(
                ids.value(i).to_string(),
                SessionAggregates {
                    title: titles.is_valid(i).then(|| titles.value(i).to_string()),
                    model: models.is_valid(i).then(|| models.value(i).to_string()),
                    message_count: message_counts.value(i),
                    input_tokens: input_tokens.value(i),
                    output_tokens: output_tokens.value(i),
                    cache_read_input_tokens: cache_read.value(i),
                    cache_creation_input_tokens: cache_creation.value(i),
                    is_empty: is_empty.value(i),
                },
            );
        }
    }
    Ok(map)
}
