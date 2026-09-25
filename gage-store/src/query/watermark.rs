//! The `scan_watermark` table: one row per `watermarks/<kind>/<oid>/<key>`
//! record across every live scan. System tier: `unseen` reads the
//! watermarks a key holds for the scan's sessions and picks, per
//! session, the closest one in the session's commit chain.

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
        // `sessions`, `notes`, or `attachments`
        utf8("kind"),
        // The Gage object id of the watermarked object
        utf8("oid"),
        utf8("key"),
        // The object's commit the task finished processing
        utf8("commit"),
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
        let mut kinds = StringBuilder::new();
        let mut oids = StringBuilder::new();
        let mut keys = StringBuilder::new();
        let mut commits = StringBuilder::new();
        for tip in scans.query().tips().map_err(external)? {
            let record = scans.at_commit(&tip.sha).map_err(external)?;
            for w in &record.content.watermarks {
                scan_ids.append_value(&tip.id);
                scan_commits.append_value(&tip.sha);
                kinds.append_value(&w.kind);
                oids.append_value(&w.oid);
                keys.append_value(&w.key);
                commits.append_value(&w.commit);
            }
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(scan_ids.finish()),
                Arc::new(scan_commits.finish()),
                Arc::new(kinds.finish()),
                Arc::new(oids.finish()),
                Arc::new(keys.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `scan_watermark` table.
pub fn scan_watermark_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
