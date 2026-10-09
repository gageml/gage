//! The `tag` table: one row per tag ref, with the id of the object
//! it names. The id is read from the tagged commit's `id` blob, one
//! read per tag; the object's current state is not read.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use gage_session::system;

use super::batch::{BatchSource, BatchTable, external};
use crate::{Store, TagStore};

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        // The named object's id
        Field::new("id", DataType::Utf8, false),
        // System: the commit the tag ref points at
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
        let targets = TagStore::from(store).targets().map_err(external)?;
        let len = targets.len();
        let mut names = StringBuilder::with_capacity(len, len * 16);
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        for target in &targets {
            names.append_value(&target.name);
            ids.append_value(&target.id);
            commits.append_value(&target.commit_sha);
        }
        Ok(RecordBatch::try_new(
            schema(),
            vec![
                Arc::new(names.finish()),
                Arc::new(ids.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

/// The store-bound `tag` table.
pub fn tag_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}
