//! The `scan_validation` table: one row per `validation/<type>/<key>/<id>`
//! record across every live scan. System tier: `partition` reads the
//! largest validator per input under a key, and carry-forward finds
//! the scan that recorded it.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

use super::batch::{BatchSource, BatchTable, external};
use crate::{ScanStore, Store};

fn schema() -> SchemaRef {
    let utf8 = |name: &str| Field::new(name, DataType::Utf8, false);
    Arc::new(Schema::new(vec![
        utf8("scan_id"),
        utf8("scan_commit"),
        utf8("input_type"),
        utf8("key"),
        utf8("input_id"),
        utf8("validator"),
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
        let mut scan_ids = StringBuilder::new();
        let mut scan_commits = StringBuilder::new();
        let mut input_types = StringBuilder::new();
        let mut keys = StringBuilder::new();
        let mut input_ids = StringBuilder::new();
        let mut validators = StringBuilder::new();
        for tip in scans.query().tips().map_err(external)? {
            let record = scans.at_commit(&tip.sha).map_err(external)?;
            for v in &record.content.validation {
                scan_ids.append_value(&tip.id);
                scan_commits.append_value(&tip.sha);
                input_types.append_value(&v.input_type);
                keys.append_value(&v.key);
                input_ids.append_value(&v.input_id);
                validators.append_value(&v.validator);
            }
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(scan_ids.finish()),
                Arc::new(scan_commits.finish()),
                Arc::new(input_types.finish()),
                Arc::new(keys.finish()),
                Arc::new(input_ids.finish()),
                Arc::new(validators.finish()),
            ],
        )?)
    }
}

/// The store-bound `scan_validation` table.
pub fn scan_validation_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
