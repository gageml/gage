use std::fmt::{self, Formatter};
use std::path::PathBuf;
use std::sync::Arc;

use crate::index::IndexStore;
use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use futures::StreamExt;

use super::cache::SessionCache;
use super::walk::{SummaryPrune, session_paths};
use gage_session::{filter, tool_use_file_schema};

/// The pushed filters the walk prunes sessions on through the summary
/// cache: a session is read only when one of its recorded `(path,
/// type)` pairs satisfies the pushed `path` and `type` terms.
const PRUNE: SummaryPrune = SummaryPrune {
    time_col: None,
    path_col: Some("path"),
    type_col: Some("type"),
};

/// The `tool_use_file` table over a corpus: one row per tool call per
/// path it names.
#[derive(Debug, Clone)]
pub struct ToolUseFileTable {
    store: Arc<IndexStore>,
    schema: SchemaRef,
}

impl ToolUseFileTable {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self {
            store,
            schema: tool_use_file_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for ToolUseFileTable {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// A filter over `session_id` alone is applied in the walk. A
    /// filter over `path` or `type` prunes the walk and is then
    /// applied by DataFusion to the rows, so it is inexact.
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
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let paths = session_paths(&self.store, filters, "session_id", PRUNE)?;
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        Ok(Arc::new(ToolUseFileExec::new(
            paths,
            SessionCache::shared(),
            projected_schema,
            projection.cloned(),
            limit,
        )))
    }
}

/// Streaming plan: pulls one session at a time and emits its
/// `tool_use_file` batch. Honors `limit` so the stream stops opening
/// sessions once enough rows have been produced.
#[derive(Clone)]
struct ToolUseFileExec {
    paths: Arc<Vec<(String, PathBuf)>>,
    cache: Arc<SessionCache>,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    limit: Option<usize>,
    properties: Arc<PlanProperties>,
}

impl fmt::Debug for ToolUseFileExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("ToolUseFileExec")
            .field("sessions", &self.paths.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl ToolUseFileExec {
    fn new(
        paths: Vec<(String, PathBuf)>,
        cache: Arc<SessionCache>,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        limit: Option<usize>,
    ) -> Self {
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
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

impl DisplayAs for ToolUseFileExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "ToolUseFileExec")
    }
}

impl ExecutionPlan for ToolUseFileExec {
    fn name(&self) -> &'static str {
        "ToolUseFileExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> Result<TreeNodeRecursion>,
    ) -> Result<TreeNodeRecursion> {
        Ok(TreeNodeRecursion::Continue)
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    #[allow(deprecated)]
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
                    let batch = derived.tool_use_files.clone();
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
