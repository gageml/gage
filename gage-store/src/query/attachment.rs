//! The `attachment` table: one row per live attachment object. The
//! markers come from the index; the attrs come from the object, read
//! for every row.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

use super::batch::{BatchSource, BatchTable, external, marker, unique_prefix_lens};
use crate::{ATTACHMENT_TYPE, AttachmentRecord, AttachmentStore, Order, Store};

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

fn schema() -> SchemaRef {
    // Column order: identity, key, value, timestamps, system
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // Key
        Field::new("name", DataType::Utf8, false),
        // Value
        // The directory the files were selected under
        Field::new("root", DataType::Utf8, false),
        // The include and exclude patterns, space-joined
        Field::new("includes", DataType::Utf8, false),
        Field::new("excludes", DataType::Utf8, false),
        Field::new("file_count", DataType::Int64, false),
        // Total bytes across the files
        Field::new("size", DataType::Int64, false),
        // Timestamps
        Field::new("created", timestamp(), false),
        Field::new("modified", timestamp(), false),
        // System
        // Shortest prefix of `id` unique among every live attachment
        Field::new("id_prefix", DataType::Utf8, false),
        // `git:<commit sha>` of the version listed
        Field::new("locator", DataType::Utf8, false),
        Field::new("commit", DataType::Utf8, false),
    ]))
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
        schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let attachments = live_attachments(store)?;
        let prefix_len = unique_prefix_lens(
            store
                .short_prefix_ids(Some(ATTACHMENT_TYPE))
                .map_err(external)?,
        );
        let len = attachments.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut names = StringBuilder::new();
        let mut roots = StringBuilder::new();
        let mut includes = StringBuilder::new();
        let mut excludes = StringBuilder::new();
        let mut file_counts = Int64Builder::with_capacity(len);
        let mut sizes = Int64Builder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        for a in &attachments {
            ids.append_value(&a.id);
            names.append_value(&a.attrs.name);
            roots.append_value(a.attrs.root.to_string_lossy());
            includes.append_value(a.attrs.includes.join(" "));
            excludes.append_value(a.attrs.excludes.join(" "));
            file_counts.append_value(a.attrs.file_count as i64);
            sizes.append_value(a.attrs.size as i64);
            createds.append_value(marker(&a.id, "created", a.created_ms)?);
            modifieds.append_value(marker(&a.id, "modified", a.modified_ms)?);
            let n = prefix_len.get(&a.id).copied().unwrap_or(a.id.len());
            id_prefixes.append_value(a.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", a.commit_sha));
            commits.append_value(&a.commit_sha);
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(names.finish()),
                Arc::new(roots.finish()),
                Arc::new(includes.finish()),
                Arc::new(excludes.finish()),
                Arc::new(file_counts.finish()),
                Arc::new(sizes.finish()),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `attachment` table.
pub fn attachment_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
