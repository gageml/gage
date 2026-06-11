use std::any::Any;
use std::path::{Path, PathBuf};
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
use super::store_scan::{reconcile_for_query, scan_store_files, store_files, unqualify};
use crate::filter::{self, IdFilter};

/// The store columns serving the `entry` table, in table-column
/// order: a projection of every store row.
const STORE_PROJECTION: &[usize] = &[
    COL_SESSION_ID,
    COL_LINE,
    COL_UUID,
    COL_TYPE,
    COL_TIMESTAMP,
    COL_RAW,
];

static ENTRY_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(
        gage_index::store_schema()
            .project(STORE_PROJECTION)
            .expect("store schema serves entry projection"),
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
            source: SessionSource::Store(store),
            schema: entry_schema(),
        }
    }

    /// Build an `EntryTable` scoped to one session file.
    pub fn with_session(session_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            source: SessionSource::SingleSession {
                session_id: session_id.into(),
                path: path.into(),
            },
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
        let support = filters
            .iter()
            .map(|f| filter::pushdown(f, "session_id"))
            .collect();
        Ok(support)
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match &self.source {
            SessionSource::Store(store) => {
                reconcile_for_query(store).await?;
                let files: Vec<PathBuf> = store_files(store, filters, "session_id")?
                    .into_iter()
                    .map(|(_, path)| path)
                    .collect();
                let store_projection = map_projection(projection);
                let forwarded: Vec<Expr> = filters
                    .iter()
                    .filter(|f| !super::store_scan::references_text_search(f))
                    .map(unqualify)
                    .collect();
                scan_store_files(state, &files, Some(store_projection), &forwarded, limit).await
            }
            SessionSource::SingleSession { session_id, path } => {
                let keep = match IdFilter::new(filters, "session_id")? {
                    Some(f) => f.matches(session_id)?,
                    None => true,
                };
                let batch = if keep {
                    derive_batch(session_id, path).await?
                } else {
                    RecordBatch::new_empty(self.schema.clone())
                };
                let mem = MemTable::try_new(self.schema.clone(), vec![vec![batch]])?;
                mem.scan(state, projection, &[], limit).await
            }
        }
    }
}

/// Map a table-schema projection onto store column indices.
fn map_projection(projection: Option<&Vec<usize>>) -> Vec<usize> {
    match projection {
        Some(indices) => indices
            .iter()
            .filter_map(|&i| STORE_PROJECTION.get(i).copied())
            .collect(),
        None => STORE_PROJECTION.to_vec(),
    }
}

/// Derive one session's entry rows in memory — the per-session
/// scanner context, which bypasses the store. An unreadable session
/// yields no rows, matching the corpus walk's behavior.
async fn derive_batch(session_id: &str, path: &Path) -> Result<RecordBatch> {
    let session_id = session_id.to_string();
    let path = path.to_path_buf();
    let derived = tokio::task::spawn_blocking(move || gage_index::derive_session(&session_id, &path))
        .await
        .map_err(|e| {
            datafusion::error::DataFusionError::Execution(format!("derive task failed: {e}"))
        })?;
    match derived {
        Ok(derived) => Ok(derived.batch.project(STORE_PROJECTION)?),
        Err(e) => {
            tracing::debug!("session not derivable: {e}");
            Ok(RecordBatch::new_empty(entry_schema()))
        }
    }
}
