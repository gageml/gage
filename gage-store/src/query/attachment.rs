//! The `attachment` table: one row per live attachment object, and
//! the `attachment_file` table: one row per file of each, with its
//! content. The markers come from the index; the attrs come from the
//! object, read for every row. The row builders are shared with the
//! scan scope, which serves both tables over one dataset's
//! attachments.
//!
//! `attachment_file` reads every file's bytes of every attachment it
//! lists on each query. Attachments are small file sets, so the cost
//! is accepted until a filter pushdown on `attachment_id` is needed.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{
    Int64Builder, ListBuilder, StringBuilder, TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

use gage_session::system;

use super::batch::{BatchSource, BatchTable, external, marker, unique_prefix_lens};
use crate::{AttachmentRecord, AttachmentStore, Order, Store, StoreError};

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

pub(crate) fn attachment_schema() -> SchemaRef {
    // Column order: identity, key, value, timestamps, system
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // Key
        Field::new("name", DataType::Utf8, true),
        // The identity a later add addresses
        Field::new("key", DataType::Utf8, true),
        // Gage URLs of the objects the files are about
        Field::new_list("targets", target_item(), false),
        // Value
        Field::new("file_count", DataType::Int64, false),
        // Total bytes across the files
        Field::new("size", DataType::Int64, false),
        // XxHash3 of the file keys and contents; null on attachments
        // written before the digest existed
        Field::new("digest", DataType::Utf8, true),
        // Timestamps
        Field::new("created", timestamp(), false),
        Field::new("modified", timestamp(), false),
        // System
        // Shortest prefix of `id` unique among the attachments listed
        system(Field::new("id_prefix", DataType::Utf8, false)),
        // `git:<commit sha>` of the version listed
        system(Field::new("locator", DataType::Utf8, false)),
        system(Field::new("commit", DataType::Utf8, false)),
        // The directory the files were selected under, on the machine
        // that added them. Writer provenance, not useful on a reader.
        system(Field::new("root", DataType::Utf8, false)),
    ]))
}

fn target_item() -> Field {
    Field::new("item", DataType::Utf8, true)
}

pub(crate) fn attachment_file_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("attachment_id", DataType::Utf8, false),
        // The path relative to the attachment's root, `/`-separated
        Field::new("path", DataType::Utf8, false),
        Field::new("size", DataType::Int64, false),
        // The file's content as UTF-8 text, or null when the bytes are
        // not valid UTF-8
        Field::new("text", DataType::Utf8, true),
    ]))
}

/// The `attachment` batch for `attachments`, each at the version its
/// record names; `id_prefix` is unique among them.
pub(crate) fn attachment_rows(attachments: &[AttachmentRecord]) -> Result<RecordBatch> {
    let prefix_len = unique_prefix_lens(attachments.iter().map(|a| a.id.clone()).collect());
    let len = attachments.len();
    let mut ids = StringBuilder::with_capacity(len, len * 26);
    let mut names = StringBuilder::new();
    let mut keys = StringBuilder::new();
    let mut targets = ListBuilder::new(StringBuilder::new()).with_field(target_item());
    let mut file_counts = Int64Builder::with_capacity(len);
    let mut sizes = Int64Builder::with_capacity(len);
    let mut digests = StringBuilder::with_capacity(len, len * 16);
    let mut createds = TimestampMillisecondBuilder::with_capacity(len);
    let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
    let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
    let mut locators = StringBuilder::with_capacity(len, len * 44);
    let mut commits = StringBuilder::with_capacity(len, len * 40);
    let mut roots = StringBuilder::new();
    for a in attachments {
        ids.append_value(&a.id);
        names.append_option(a.attrs.name.as_deref());
        keys.append_option(a.attrs.natural_key.as_deref());
        for target in &a.attrs.targets {
            targets.values().append_value(target);
        }
        targets.append(true);
        file_counts.append_value(a.attrs.file_count as i64);
        sizes.append_value(a.attrs.size as i64);
        digests.append_option(a.attrs.digest.as_deref());
        createds.append_value(marker(&a.id, "created", a.created_ms)?);
        modifieds.append_value(marker(&a.id, "modified", a.modified_ms)?);
        let n = prefix_len.get(&a.id).copied().unwrap_or(a.id.len());
        id_prefixes.append_value(a.id.chars().take(n).collect::<String>());
        locators.append_value(format!("git:{}", a.commit_sha));
        commits.append_value(&a.commit_sha);
        roots.append_value(a.attrs.root.to_string_lossy());
    }
    Ok(RecordBatch::try_new(
        attachment_schema(),
        vec![
            Arc::new(ids.finish()),
            Arc::new(names.finish()),
            Arc::new(keys.finish()),
            Arc::new(targets.finish()),
            Arc::new(file_counts.finish()),
            Arc::new(sizes.finish()),
            Arc::new(digests.finish()),
            Arc::new(createds.finish().with_timezone("UTC")),
            Arc::new(modifieds.finish().with_timezone("UTC")),
            Arc::new(id_prefixes.finish()),
            Arc::new(locators.finish()),
            Arc::new(commits.finish()),
            Arc::new(roots.finish()),
        ],
    )?)
}

/// The `attachment_file` batch: every file of each of `attachments`,
/// in tree order, with its content as UTF-8 text; non-UTF-8 bytes
/// yield null.
pub(crate) fn attachment_file_rows(
    store: &Store,
    attachments: &[AttachmentRecord],
) -> Result<RecordBatch> {
    let files = AttachmentStore::from(store);
    let mut attachment_ids = StringBuilder::new();
    let mut paths = StringBuilder::new();
    let mut sizes = Int64Builder::new();
    let mut texts = StringBuilder::new();
    for a in attachments {
        for file in files.files(&a.commit_sha).map_err(external)? {
            let bytes = files
                .read_file(&a.commit_sha, &file.key)
                .map_err(external)?
                .ok_or_else(|| {
                    external(StoreError::Parse(format!(
                        "attachment {} lists {} but holds no such file",
                        a.id, file.key
                    )))
                })?;
            attachment_ids.append_value(&a.id);
            paths.append_value(&file.key);
            sizes.append_value(file.size as i64);
            match String::from_utf8(bytes) {
                Ok(s) => texts.append_value(&s),
                Err(_) => texts.append_null(),
            }
        }
    }
    Ok(RecordBatch::try_new(
        attachment_file_schema(),
        vec![
            Arc::new(attachment_ids.finish()),
            Arc::new(paths.finish()),
            Arc::new(sizes.finish()),
            Arc::new(texts.finish()),
        ],
    )?)
}

/// Every live attachment, newest modified first.
fn live_attachments(store: &Store) -> Result<Vec<AttachmentRecord>> {
    AttachmentStore::from(store)
        .query()
        .order(Order::ModifiedDesc)
        .iter()
        .and_then(|iter| iter.collect())
        .map_err(external)
}

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        attachment_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        attachment_rows(&live_attachments(store)?)
    }
}

#[derive(Debug)]
struct FileSource;

impl BatchSource for FileSource {
    fn schema(&self) -> SchemaRef {
        attachment_file_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        attachment_file_rows(store, &live_attachments(store)?)
    }
}

/// The store-bound `attachment` table.
pub fn attachment_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}

/// The store-bound `attachment_file` table.
pub fn attachment_file_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(FileSource)))
}
