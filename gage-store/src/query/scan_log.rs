//! The `scan_log` table: one row per log a stored scan holds, `out`,
//! `err`, or `records`, with its content. The schema and row builder
//! are shared with the scan scope, which serves an active scan's logs
//! from its directory.
//!
//! The table reads every log of every scan it lists on each query. A
//! scan's logs are a few small files, so the cost is accepted until a
//! filter pushdown on `scan_id` is needed.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

use super::batch::{BatchSource, BatchTable, external, readable};
use crate::{Order, ScanRecord, ScanStore, Store, StoreError};

pub(crate) fn scan_log_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("scan_id", DataType::Utf8, false),
        // One of `out`, `err`, `records`
        Field::new("name", DataType::Utf8, false),
        // The log's content; bytes that are not UTF-8 are replaced
        Field::new("text", DataType::Utf8, false),
    ]))
}

/// One `scan_log` row from either scope.
pub(crate) struct ScanLogRow {
    pub scan_id: String,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The `scan_log` batch for `rows`.
pub(crate) fn scan_log_rows(rows: &[ScanLogRow]) -> Result<RecordBatch> {
    let mut scan_ids = StringBuilder::new();
    let mut names = StringBuilder::new();
    let mut texts = StringBuilder::new();
    for row in rows {
        scan_ids.append_value(&row.scan_id);
        names.append_value(&row.name);
        texts.append_value(String::from_utf8_lossy(&row.bytes));
    }
    Ok(RecordBatch::try_new(
        scan_log_schema(),
        vec![
            Arc::new(scan_ids.finish()),
            Arc::new(names.finish()),
            Arc::new(texts.finish()),
        ],
    )?)
}

/// The rows of a stored scan's logs, in the order the object lists
/// them.
pub(crate) fn stored_scan_log_rows(store: &Store, record: &ScanRecord) -> Result<Vec<ScanLogRow>> {
    let scans = ScanStore::from(store);
    let mut rows = Vec::with_capacity(record.content.logs.len());
    for name in &record.content.logs {
        let bytes = scans
            .scan_log(&record.commit_sha, name)
            .map_err(external)?
            .ok_or_else(|| {
                external(StoreError::Parse(format!(
                    "scan {} lists log {name} but holds no such file",
                    record.id
                )))
            })?;
        rows.push(ScanLogRow {
            scan_id: record.id.clone(),
            name: name.clone(),
            bytes,
        });
    }
    Ok(rows)
}

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        scan_log_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let scans = ScanStore::from(store);
        let tips = scans
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let mut rows = Vec::new();
        for tip in &tips {
            let Some(record) = readable(scans.at_commit(&tip.sha))? else {
                continue;
            };
            rows.extend(stored_scan_log_rows(store, &record)?);
        }
        scan_log_rows(&rows)
    }
}

/// The store-bound `scan_log` table.
pub fn scan_log_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
