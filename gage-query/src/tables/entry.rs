use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use futures::StreamExt;
use gage_index::{
    COL_LINE, COL_RAW, COL_SESSION_ID, COL_SUBTYPE, COL_TIMESTAMP, COL_TYPE, COL_UUID, IndexStore,
};

use super::SessionSource;
use super::walk::{lookup_paths, session_cache, session_paths};
use crate::cache::SessionCache;
use crate::filter;

const PROJECTION: &[usize] = &[
    COL_SESSION_ID,
    COL_LINE,
    COL_UUID,
    COL_TYPE,
    COL_SUBTYPE,
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
        // Intentionally limit pushdowns to session_id
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
        let paths = match &self.source {
            SessionSource::Corpus(store) => session_paths(state, store, filters, "session_id")?,
            SessionSource::Lookup(sessions) => lookup_paths(sessions, filters)?,
        };
        let cache = session_cache(state)?;
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        Ok(Arc::new(EntryExec::new(
            paths,
            cache,
            projected_schema,
            projection.cloned(),
            limit,
        )))
    }
}

/// Streaming plan: pulls one session at a time, projects its derived
/// batch to the entry schema, and emits it. Honors `limit` so the
/// stream stops opening sessions once enough rows have been produced.
#[derive(Clone)]
struct EntryExec {
    paths: Arc<Vec<(String, PathBuf)>>,
    cache: Arc<SessionCache>,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for EntryExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("EntryExec")
            .field("sessions", &self.paths.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl EntryExec {
    fn new(
        paths: Vec<(String, PathBuf)>,
        cache: Arc<SessionCache>,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        limit: Option<usize>,
    ) -> Self {
        let properties = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            paths: Arc::new(paths),
            cache,
            projected_schema,
            projection,
            limit,
            properties,
        }
    }
}

impl DisplayAs for EntryExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "EntryExec")
    }
}

impl ExecutionPlan for EntryExec {
    fn name(&self) -> &'static str {
        "EntryExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn fetch(&self) -> Option<usize> {
        self.limit
    }

    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        let mut next = self.clone();
        next.limit = limit;
        Some(Arc::new(next))
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let paths = (*self.paths).clone();
        let cache = self.cache.clone();
        let projection = self.projection.clone();
        let schema = self.projected_schema.clone();
        let limit = self.limit;

        let batches = futures::stream::iter(paths)
            .then(move |(id, path)| {
                let cache = cache.clone();
                async move {
                    let result = cache.get(&id, &path).await;
                    (id, result)
                }
            })
            .filter_map(|(id, result)| async move {
                match result {
                    Ok(derived) => Some(Ok(derived)),
                    Err(e) => {
                        tracing::warn!(session_id = %id, "skipping session: {e}");
                        None
                    }
                }
            })
            .map(move |res| {
                res.and_then(|derived| {
                    let projected = derived.batch.project(PROJECTION)?;
                    match &projection {
                        Some(p) => projected.project(p).map_err(DataFusionError::from),
                        None => Ok(projected),
                    }
                })
            });

        let stream: futures::stream::BoxStream<'static, Result<RecordBatch>> = match limit {
            Some(n) => batches
                .scan(n, |remaining, res| {
                    let item = if *remaining == 0 {
                        None
                    } else {
                        Some(res.map(|batch| {
                            if batch.num_rows() > *remaining {
                                let sliced = batch.slice(0, *remaining);
                                *remaining = 0;
                                sliced
                            } else {
                                *remaining -= batch.num_rows();
                                batch
                            }
                        }))
                    };
                    futures::future::ready(item)
                })
                .boxed(),
            None => batches.boxed(),
        };

        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }
}
