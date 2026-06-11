use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::Result;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::*;
use gage_index::{
    COL_LINE, COL_RAW, COL_SESSION_ID, COL_TIMESTAMP, COL_TYPE, COL_UUID, IndexStore,
};

use super::SessionSource;
use super::walk::{lookup_paths, reconcile_for_query, session_cache, session_paths};
use crate::cache::SessionCache;
use crate::filter;

const PROJECTION: &[usize] = &[
    COL_SESSION_ID,
    COL_LINE,
    COL_UUID,
    COL_TYPE,
    COL_TIMESTAMP,
    COL_RAW,
];

static ENTRY_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(
        gage_index::derived_schema()
            .project(PROJECTION)
            .expect("derived schema serves entry projection"),
    )
});

fn entry_schema() -> SchemaRef {
    ENTRY_SCHEMA.clone()
}

#[derive(Debug, Clone)]
pub struct EntryTable {
    source: SessionSource,
    schema: SchemaRef,
}

impl EntryTable {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self {
            source: SessionSource::Corpus(store),
            schema: entry_schema(),
        }
    }

    /// Build an `EntryTable` over an explicit `session_id` -> path map.
    /// Mirrors [`crate::tables::MessageTable::with_lookup`].
    pub fn with_lookup(sessions: Arc<HashMap<String, PathBuf>>) -> Self {
        Self {
            source: SessionSource::Lookup(sessions),
            schema: entry_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for EntryTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<datafusion::logical_expr::TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| filter::pushdown(f, "session_id"))
            .collect())
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let batches = match &self.source {
            SessionSource::Corpus(store) => {
                reconcile_for_query(store).await?;
                let cache = session_cache(state)?;
                let paths = session_paths(store, filters, "session_id")?;
                load_batches(&cache, paths).await?
            }
            SessionSource::Lookup(sessions) => {
                let cache = session_cache(state)?;
                let paths = lookup_paths(sessions, filters)?;
                load_batches(&cache, paths).await?
            }
        };
        let mem = MemTable::try_new(self.schema.clone(), vec![batches])?;
        mem.scan(state, projection, &[], limit).await
    }
}

async fn load_batches(
    cache: &Arc<SessionCache>,
    paths: Vec<(String, PathBuf)>,
) -> Result<Vec<RecordBatch>> {
    let mut batches = Vec::with_capacity(paths.len());
    for (id, path) in paths {
        let derived = match cache.get(&id, &path).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(session_id = %id, "skipping session: {e}");
                continue;
            }
        };
        batches.push(derived.batch.project(PROJECTION)?);
    }
    Ok(batches)
}
