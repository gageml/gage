//! The `scan` table: one row per live scan object. The markers come
//! from the index; the run attributes and the dataset id come from
//! the object, read for every row.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{
    BooleanBuilder, Int64Builder, StringBuilder, TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use gage_core::uuid::short_uuid;

use super::batch::{BatchSource, BatchTable, external, marker, unique_prefix_lens};
use crate::{Order, SCAN_TYPE, ScanStore, Store};

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("modified", timestamp(), false),
        Field::new("created", timestamp(), false),
        // The run
        Field::new("runtime", DataType::Utf8, false),
        Field::new("started", timestamp(), false),
        Field::new("stopped", timestamp(), false),
        Field::new("canceled", DataType::Boolean, false),
        Field::new("tasks", DataType::Int64, false),
        Field::new("completed", DataType::Int64, false),
        Field::new("failed", DataType::Int64, false),
        Field::new("skipped", DataType::Int64, false),
        // The dataset scanned; null when the scan had none
        Field::new("dataset", DataType::Utf8, true),
        // System
        Field::new("id_display", DataType::Utf8, false),
        Field::new("id_prefix", DataType::Utf8, false),
        Field::new("locator", DataType::Utf8, false),
        Field::new("commit", DataType::Utf8, false),
        // The dataset commit the scan read, from `dataset.link`
        Field::new("dataset_commit", DataType::Utf8, true),
    ]))
}

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let scans = ScanStore::from(store);
        let tips = scans
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let prefix_len =
            unique_prefix_lens(store.short_prefix_ids(Some(SCAN_TYPE)).map_err(external)?);
        let len = tips.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut runtimes = StringBuilder::new();
        let mut starteds = TimestampMillisecondBuilder::with_capacity(len);
        let mut stoppeds = TimestampMillisecondBuilder::with_capacity(len);
        let mut canceleds = BooleanBuilder::with_capacity(len);
        let mut tasks = Int64Builder::with_capacity(len);
        let mut completeds = Int64Builder::with_capacity(len);
        let mut faileds = Int64Builder::with_capacity(len);
        let mut skippeds = Int64Builder::with_capacity(len);
        let mut datasets = StringBuilder::new();
        let mut id_displays = StringBuilder::with_capacity(len, len * 8);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        let mut dataset_commits = StringBuilder::new();
        for tip in &tips {
            let record = scans.at_commit(&tip.sha).map_err(external)?;
            let attrs = &record.content.attrs;
            ids.append_value(&tip.id);
            modifieds.append_value(marker(&tip.id, "modified", tip.modified_ms)?);
            createds.append_value(marker(&tip.id, "created", tip.created_ms)?);
            runtimes.append_value(&attrs.runtime);
            starteds.append_value(attrs.started);
            stoppeds.append_value(attrs.stopped);
            canceleds.append_value(attrs.canceled);
            tasks.append_value(attrs.tasks.total as i64);
            completeds.append_value(attrs.tasks.completed as i64);
            faileds.append_value(attrs.tasks.failed as i64);
            skippeds.append_value(attrs.tasks.skipped as i64);
            match &record.content.dataset {
                Some(sha) => {
                    let header = store.read_header(sha).map_err(external)?;
                    datasets.append_value(header.id);
                    dataset_commits.append_value(sha);
                }
                None => {
                    datasets.append_null();
                    dataset_commits.append_null();
                }
            }
            id_displays.append_value(short_uuid(&tip.id));
            let n = prefix_len.get(&tip.id).copied().unwrap_or(tip.id.len());
            id_prefixes.append_value(tip.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", tip.sha));
            commits.append_value(&tip.sha);
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(runtimes.finish()),
                Arc::new(starteds.finish().with_timezone("UTC")),
                Arc::new(stoppeds.finish().with_timezone("UTC")),
                Arc::new(canceleds.finish()),
                Arc::new(tasks.finish()),
                Arc::new(completeds.finish()),
                Arc::new(faileds.finish()),
                Arc::new(skippeds.finish()),
                Arc::new(datasets.finish()),
                Arc::new(id_displays.finish()),
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
                Arc::new(commits.finish()),
                Arc::new(dataset_commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `scan` table.
pub fn scan_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
