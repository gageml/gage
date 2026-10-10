//! The `object_key` table: one row per key ref, with the id of the
//! object it names. The id is read from the keyed commit's `id` blob,
//! one read per ref; the object's current state is not read.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use gage_session::system;

use super::batch::{BatchSource, BatchTable, external};
use crate::{KeyStore, Store};

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        // The bare type name the key is filed under: issue, attachment
        Field::new("type", DataType::Utf8, false),
        Field::new("key", DataType::Utf8, false),
        // The named object's id
        Field::new("id", DataType::Utf8, false),
        // System: the commit the key ref points at
        system(Field::new("commit", DataType::Utf8, false)),
    ]))
}

#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let refs = KeyStore::from(store).list().map_err(external)?;
        let len = refs.len();
        let mut types = StringBuilder::with_capacity(len, len * 10);
        let mut keys = StringBuilder::with_capacity(len, len * 24);
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        for r in &refs {
            types.append_value(&r.type_name);
            keys.append_value(&r.key);
            ids.append_value(&r.id);
            commits.append_value(&r.commit_sha);
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(types.finish()),
                Arc::new(keys.finish()),
                Arc::new(ids.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `object_key` table.
pub fn object_key_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
