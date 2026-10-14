use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::path::PathBuf;
use std::sync::Arc;

use crate::index::IndexStore;
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

use super::SessionSource;
use super::cache::SessionCache;
use super::walk::{lookup_paths, session_paths};
use gage_session::{filter, tool_use_schema};

/// Positions of `session_id` and `line` in the `tool_use` schema, the
/// two columns a `RowFilter` masks on
const COL_SESSION_ID: usize = 0;
const COL_LINE: usize = 3;

#[derive(Debug, Clone)]
pub struct ToolUseTable {
    source: SessionSource,
    schema: SchemaRef,
}

impl ToolUseTable {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self {
            source: SessionSource::Corpus(store),
            schema: tool_use_schema(),
        }
    }

    /// Build a `ToolUseTable` over an explicit `session_id` -> path
    /// map. Mirrors [`crate::tables::MessageTable::with_lookup`].
    pub fn with_lookup(sessions: Arc<HashMap<String, PathBuf>>) -> Self {
        Self {
            source: SessionSource::Lookup(sessions),
            schema: tool_use_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for ToolUseTable {
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
        // Pushdowns limited to session_id (path pruning) and line
        // (row masking); everything else needs the parsed columns
        Ok(filters
            .iter()
            .map(|f| filter::pushdown_lines(f, "session_id"))
            .collect())
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let paths = match &self.source {
            SessionSource::Corpus(store) => session_paths(store, filters, "session_id")?,
            SessionSource::Lookup(sessions) => lookup_paths(sessions, filters)?,
        };
        let row_filter = filter::RowFilter::new(filters, "session_id")?;
        let cache = SessionCache::shared();
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        Ok(Arc::new(ToolUseExec::new(
            paths,
            cache,
            projected_schema,
            projection.cloned(),
            row_filter,
            limit,
        )))
    }
}

/// Streaming plan: pulls one session at a time and emits its
/// `tool_use` batch. Honors `limit` so the stream stops opening
/// sessions once enough rows have been produced.
#[derive(Clone)]
struct ToolUseExec {
    paths: Arc<Vec<(String, PathBuf)>>,
    cache: Arc<SessionCache>,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    row_filter: Option<filter::RowFilter>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for ToolUseExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("ToolUseExec")
            .field("sessions", &self.paths.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl ToolUseExec {
    fn new(
        paths: Vec<(String, PathBuf)>,
        cache: Arc<SessionCache>,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        row_filter: Option<filter::RowFilter>,
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
            row_filter,
            limit,
            properties,
        }
    }
}

impl DisplayAs for ToolUseExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "ToolUseExec")
    }
}

impl ExecutionPlan for ToolUseExec {
    fn name(&self) -> &'static str {
        "ToolUseExec"
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
        let row_filter = self.row_filter.clone();
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
                    let mut batch = derived.tool_uses.clone();
                    if let Some(f) = &row_filter {
                        batch = f.filter_batch(&batch, COL_SESSION_ID, COL_LINE)?;
                    }
                    match &projection {
                        Some(p) => batch.project(p).map_err(DataFusionError::from),
                        None => Ok(batch),
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
