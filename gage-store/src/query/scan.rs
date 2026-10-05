//! The `scan` table: one row per live scan object. The markers come
//! from the index; the run attributes and the dataset id come from
//! the object, read for every row. The schema and row builder are
//! shared with the scan scope, whose one row may describe an active
//! scan: no markers, no commit, and a run that has not stopped, so
//! those columns are nullable.

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
use crate::{Order, SCAN_TYPE, ScanRecord, ScanStore, Store};

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

pub(crate) fn scan_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        // The markers; null for an active scan, which is not an object yet
        Field::new("modified", timestamp(), true),
        Field::new("created", timestamp(), true),
        // The run; `runtime`, `stopped`, and `canceled` are null for
        // an active scan
        Field::new("runtime", DataType::Utf8, true),
        Field::new("started", timestamp(), true),
        Field::new("stopped", timestamp(), true),
        Field::new("canceled", DataType::Boolean, true),
        Field::new("tasks", DataType::Int64, false),
        Field::new("completed", DataType::Int64, false),
        Field::new("failed", DataType::Int64, false),
        Field::new("skipped", DataType::Int64, false),
        // The dataset scanned; null when the scan had none
        Field::new("dataset", DataType::Utf8, true),
        // System
        Field::new("id_display", DataType::Utf8, false),
        Field::new("id_prefix", DataType::Utf8, false),
        // Null for an active scan
        Field::new("locator", DataType::Utf8, true),
        Field::new("commit", DataType::Utf8, true),
        // The dataset commit the scan read, from `dataset.link`
        Field::new("dataset_commit", DataType::Utf8, true),
    ]))
}

/// One `scan` row from either scope.
pub(crate) struct ScanRow {
    pub id: String,
    pub modified_ms: Option<i64>,
    pub created_ms: Option<i64>,
    pub runtime: Option<String>,
    pub started_ms: Option<i64>,
    pub stopped_ms: Option<i64>,
    pub canceled: Option<bool>,
    pub tasks: i64,
    pub completed: i64,
    pub failed: i64,
    pub skipped: i64,
    /// The dataset's id and the commit the scan read
    pub dataset: Option<(String, String)>,
    pub id_prefix: String,
    pub commit: Option<String>,
}

impl ScanRow {
    /// The row of a stored scan. `dataset` is the dataset's id when
    /// the scan has one.
    pub(crate) fn from_record(
        record: &ScanRecord,
        dataset_id: Option<String>,
        id_prefix: String,
    ) -> Self {
        let attrs = &record.content.attrs;
        ScanRow {
            id: record.id.clone(),
            modified_ms: Some(record.modified_ms),
            created_ms: Some(record.created_ms),
            runtime: Some(attrs.runtime.clone()),
            started_ms: Some(attrs.started),
            stopped_ms: Some(attrs.stopped),
            canceled: Some(attrs.canceled),
            tasks: attrs.tasks.total as i64,
            completed: attrs.tasks.completed as i64,
            failed: attrs.tasks.failed as i64,
            skipped: attrs.tasks.skipped as i64,
            dataset: dataset_id.zip(record.content.dataset.clone()),
            id_prefix,
            commit: Some(record.commit_sha.clone()),
        }
    }
}

/// The `scan` batch for `rows`.
pub(crate) fn scan_rows(rows: &[ScanRow]) -> Result<RecordBatch> {
    let len = rows.len();
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
    for row in rows {
        ids.append_value(&row.id);
        modifieds.append_option(row.modified_ms);
        createds.append_option(row.created_ms);
        runtimes.append_option(row.runtime.as_deref());
        starteds.append_option(row.started_ms);
        stoppeds.append_option(row.stopped_ms);
        canceleds.append_option(row.canceled);
        tasks.append_value(row.tasks);
        completeds.append_value(row.completed);
        faileds.append_value(row.failed);
        skippeds.append_value(row.skipped);
        datasets.append_option(row.dataset.as_ref().map(|(id, _)| id.as_str()));
        dataset_commits.append_option(row.dataset.as_ref().map(|(_, sha)| sha.as_str()));
        id_displays.append_value(short_uuid(&row.id));
        id_prefixes.append_value(&row.id_prefix);
        locators.append_option(row.commit.as_ref().map(|c| format!("git:{c}")));
        commits.append_option(row.commit.as_deref());
    }
    Ok(RecordBatch::try_new(
        scan_schema(),
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

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        scan_schema()
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
        let mut rows = Vec::with_capacity(tips.len());
        for tip in &tips {
            let record = scans.at_commit(&tip.sha).map_err(external)?;
            let dataset_id = match &record.content.dataset {
                Some(sha) => Some(store.read_header(sha).map_err(external)?.id),
                None => None,
            };
            let n = prefix_len.get(&tip.id).copied().unwrap_or(tip.id.len());
            let mut row =
                ScanRow::from_record(&record, dataset_id, tip.id.chars().take(n).collect());
            row.modified_ms = Some(marker(&tip.id, "modified", tip.modified_ms)?);
            row.created_ms = Some(marker(&tip.id, "created", tip.created_ms)?);
            rows.push(row);
        }
        scan_rows(&rows)
    }
}

/// The store-bound `scan` table.
pub fn scan_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
