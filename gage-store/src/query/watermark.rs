//! The `scan_watermark` table: one row per `watermarks/<oid>/<key>`
//! record across every live scan. System tier: the runtime's `hwm`
//! reads the marks a key holds for a scan's objects and takes, per
//! object, the highest one at a version the object has: on its
//! commit chain, or for an attachment at its content digest.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{StringBuilder, UInt64Builder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use gage_session::system;

use super::batch::{BatchSource, BatchTable, external};
use crate::{ScanStore, Store};

fn schema() -> SchemaRef {
    let utf8 = |name: &str| Field::new(name, DataType::Utf8, false);
    Arc::new(Schema::new(vec![
        utf8("scan_id"),
        system(utf8("scan_commit")),
        // The Gage object id of the watermarked object
        utf8("oid"),
        utf8("key"),
        // The version of the object the task read: its commit, or
        // for an attachment its content digest
        utf8("version"),
        // The position the task reached, on the object's axis
        Field::new("mark", DataType::UInt64, false),
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
        let mut oids = StringBuilder::new();
        let mut keys = StringBuilder::new();
        let mut versions = StringBuilder::new();
        let mut marks = UInt64Builder::new();
        for tip in scans.query().tips().map_err(external)? {
            let record = scans.at_commit(&tip.sha).map_err(external)?;
            for w in &record.content.watermarks {
                scan_ids.append_value(&tip.id);
                scan_commits.append_value(&tip.sha);
                oids.append_value(&w.oid);
                keys.append_value(&w.key);
                versions.append_value(&w.version);
                marks.append_value(w.mark);
            }
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(scan_ids.finish()),
                Arc::new(scan_commits.finish()),
                Arc::new(oids.finish()),
                Arc::new(keys.finish()),
                Arc::new(versions.finish()),
                Arc::new(marks.finish()),
            ],
        )?)
    }
}

/// The store-bound `scan_watermark` table.
pub fn scan_watermark_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
