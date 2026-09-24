//! A table provider over one batch computed on each scan.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_core::style::IdHighlighter;

use crate::{Store, StoreError};

/// The rows of a table, built from the store on demand.
pub(crate) trait BatchSource: Send + Sync + std::fmt::Debug {
    fn schema(&self) -> SchemaRef;
    fn build(&self, store: &Store) -> Result<RecordBatch>;
}

/// A provider that builds its rows under the store lock on every
/// scan and hands them to DataFusion as an in-memory table, which
/// applies the projection, filters, and limit.
#[derive(Debug)]
pub(crate) struct BatchTable {
    store: Arc<Mutex<Store>>,
    source: Arc<dyn BatchSource>,
}

impl BatchTable {
    pub(crate) fn new(store: Arc<Mutex<Store>>, source: Arc<dyn BatchSource>) -> Self {
        Self { store, source }
    }
}

#[async_trait]
impl TableProvider for BatchTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.source.schema()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let batch = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.source.build(&store)?
        };
        MemTable::try_new(self.source.schema(), vec![vec![batch]])?
            .scan(state, projection, filters, limit)
            .await
    }
}

pub(crate) fn external(e: StoreError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

/// The length of each id's shortest unique prefix within `ids`.
pub(crate) fn unique_prefix_lens(ids: Vec<String>) -> HashMap<String, usize> {
    let highlighter = IdHighlighter::new(ids.clone());
    ids.into_iter()
        .map(|id| {
            let n = highlighter.unique_prefix_len(&id);
            (id, n)
        })
        .collect()
}

/// The value of a required marker, or an error naming the object.
pub(crate) fn marker(id: &str, name: &str, value: Option<i64>) -> Result<i64> {
    value.ok_or_else(|| external(StoreError::Parse(format!("{id}: missing {name} marker"))))
}
