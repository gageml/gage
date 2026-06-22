use std::any::Any;
use std::fmt::{self, Formatter};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use datafusion::arrow::array::{
    BooleanBuilder, Int64Builder, StringBuilder, TimestampMillisecondBuilder,
};
use datafusion::arrow::compute::SortOptions;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::Result;
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_expr::expressions::col;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalSortExpr};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use futures::stream;
use gage_claude::session::SessionInfo;
use gage_index::{IndexStore, SessionSummary};

use super::walk::{SessionWalker, session_cache, session_walker};
use crate::cache::SessionCache;
use crate::filter;

/// Index of the first column whose value comes from parsing the
/// session JSONL (`title` and everything after). Columns before this
/// are filled from the directory walk. A projection that touches none
/// of these — `SELECT COUNT(*)`, `SELECT id FROM session WHERE ...` —
/// can skip per-row derivation entirely.
const SUMMARY_COL_START: usize = 5;

fn session_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("project", DataType::Utf8, true),
        Field::new("path", DataType::Utf8, false),
        Field::new(
            "mtime",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new("size", DataType::Int64, false),
        Field::new("title", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("message_count", DataType::Int64, false),
        Field::new("input_tokens", DataType::Int64, false),
        Field::new("output_tokens", DataType::Int64, false),
        Field::new("cache_read_input_tokens", DataType::Int64, false),
        Field::new("cache_creation_input_tokens", DataType::Int64, false),
        Field::new("is_empty", DataType::Boolean, false),
    ]))
}

#[derive(Debug, Clone)]
pub struct SessionTable {
    store: Arc<IndexStore>,
    schema: SchemaRef,
}

impl SessionTable {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self {
            store,
            schema: session_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for SessionTable {
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
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters.iter().map(|f| filter::pushdown(f, "id")).collect())
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        let cache = session_cache(state)?;
        let walker = session_walker(state);
        Ok(Arc::new(SessionExec::new(
            self.store.clone(),
            cache,
            walker,
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
            filters.to_vec(),
            limit,
        )))
    }
}

#[derive(Clone)]
struct SessionExec {
    store: Arc<IndexStore>,
    cache: Arc<SessionCache>,
    walker: SessionWalker,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for SessionExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("SessionExec")
            .field("limit", &self.limit)
            .field("filters", &self.filters)
            .finish()
    }
}

impl SessionExec {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: Arc<IndexStore>,
        cache: Arc<SessionCache>,
        walker: SessionWalker,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        filters: Vec<Expr>,
        limit: Option<usize>,
    ) -> Self {
        let eq_properties = mtime_desc_eq_properties(&projected_schema);
        let properties = PlanProperties::new(
            eq_properties,
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            store,
            cache,
            walker,
            full_schema,
            projected_schema,
            projection,
            filters,
            limit,
            properties,
        }
    }

    /// True when no projected column requires parsing the session
    /// JSONL. Lets `SELECT COUNT(*)` and id-only filters scan at
    /// directory-walk speed regardless of cache state.
    fn projection_needs_summary(&self) -> bool {
        match &self.projection {
            Some(indices) => indices.iter().any(|&i| i >= SUMMARY_COL_START),
            None => true,
        }
    }
}

/// Build `EquivalenceProperties` advertising `[mtime DESC]` when the
/// projection retains the `mtime` column. `SessionListBuilder::build`
/// already sorts by mtime descending, so this is the natural order.
/// Advertising it lets DataFusion push `LIMIT N` past `ORDER BY mtime
/// DESC`, which we honor by capping the directory walk at N entries.
fn mtime_desc_eq_properties(projected_schema: &SchemaRef) -> EquivalenceProperties {
    match col("mtime", projected_schema) {
        Ok(mtime_expr) => {
            // SQL `ORDER BY mtime DESC` in DataFusion defaults to NULLS
            // FIRST; advertise the same so EnforceSorting can elide the
            // SortExec and LimitPushdown can pass `fetch` to scan().
            let sort = PhysicalSortExpr {
                expr: mtime_expr,
                options: SortOptions {
                    descending: true,
                    nulls_first: true,
                },
            };
            EquivalenceProperties::new_with_orderings(projected_schema.clone(), [[sort]])
        }
        Err(_) => EquivalenceProperties::new(projected_schema.clone()),
    }
}

impl DisplayAs for SessionExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "SessionExec")
    }
}

impl ExecutionPlan for SessionExec {
    fn name(&self) -> &'static str {
        "SessionExec"
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

    /// Accept a fetch from DataFusion's LimitPushdown. Combined with
    /// our advertised `[mtime DESC]` ordering, this is what lets
    /// `ORDER BY mtime DESC LIMIT N` open at most N session files.
    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        Some(Arc::new(SessionExec::new(
            self.store.clone(),
            self.cache.clone(),
            self.walker.clone(),
            self.full_schema.clone(),
            self.projected_schema.clone(),
            self.projection.clone(),
            self.filters.clone(),
            limit,
        )))
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let work = self.clone();
        let schema = self.projected_schema.clone();
        let stream = stream::once(async move { work.build_batch().await });
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }
}

impl SessionExec {
    async fn build_batch(self) -> Result<RecordBatch> {
        let exec_start = std::time::Instant::now();
        let outcome = self
            .walker
            .walk(&self.store, &self.filters, "id", self.limit)?;
        let sessions: Vec<SessionInfo> = outcome.sessions;

        let needs_summary = self.projection_needs_summary();
        tracing::debug!(
            target: "gage_query::session",
            walked = outcome.walked,
            after_filters = sessions.len(),
            walk_ms = outcome.walk_ms,
            walk_limit_applied = ?outcome.limit_pushed.then_some(self.limit).flatten(),
            needs_summary,
            "SessionExec walk complete",
        );

        let len = sessions.len();
        let mut ids = StringBuilder::with_capacity(len, len * 36);
        let mut projects = StringBuilder::with_capacity(len, len * 32);
        let mut paths = StringBuilder::with_capacity(len, len * 64);
        let mut mtimes = TimestampMillisecondBuilder::with_capacity(len);
        let mut sizes = Int64Builder::with_capacity(len);
        let mut titles = StringBuilder::new();
        let mut models = StringBuilder::new();
        let mut message_counts = Int64Builder::with_capacity(len);
        let mut input_tokens = Int64Builder::with_capacity(len);
        let mut output_tokens = Int64Builder::with_capacity(len);
        let mut cache_read = Int64Builder::with_capacity(len);
        let mut cache_creation = Int64Builder::with_capacity(len);
        let mut is_empty = BooleanBuilder::with_capacity(len);

        let default_summary = SessionSummary {
            is_empty: true,
            ..Default::default()
        };

        for s in &sessions {
            ids.append_value(&s.id);
            projects.append_value(s.project_name().as_ref());
            paths.append_value(s.src.to_string_lossy().as_ref());
            let millis = s
                .mtime
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            mtimes.append_value(millis);
            sizes.append_value(s.size as i64);

            let summary = if !needs_summary {
                default_summary.clone()
            } else {
                match self.store.session_summary(&s.id, s.mtime) {
                    Some(cached) => cached,
                    None => match self.cache.get(&s.id, &s.src).await {
                        Ok(d) => {
                            // Populate the on-disk cache so subsequent
                            // queries hit `session_summary` instead of
                            // re-parsing the JSONL.
                            if let Err(e) = self.store.put_session_summary(&s.id, &d.summary) {
                                tracing::warn!(session_id = %s.id, "failed to write summary cache: {e}");
                            }
                            d.summary.clone()
                        }
                        Err(e) => {
                            tracing::warn!(session_id = %s.id, "session summary unavailable: {e}");
                            default_summary.clone()
                        }
                    },
                }
            };
            match &summary.title {
                Some(t) => titles.append_value(t),
                None => titles.append_null(),
            }
            match &summary.model {
                Some(m) => models.append_value(m),
                None => models.append_null(),
            }
            message_counts.append_value(summary.message_count);
            input_tokens.append_value(summary.input_tokens);
            output_tokens.append_value(summary.output_tokens);
            cache_read.append_value(summary.cache_read_input_tokens);
            cache_creation.append_value(summary.cache_creation_input_tokens);
            is_empty.append_value(summary.is_empty);
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(projects.finish()),
                Arc::new(paths.finish()),
                Arc::new(mtimes.finish().with_timezone("UTC")),
                Arc::new(sizes.finish()),
                Arc::new(titles.finish()),
                Arc::new(models.finish()),
                Arc::new(message_counts.finish()),
                Arc::new(input_tokens.finish()),
                Arc::new(output_tokens.finish()),
                Arc::new(cache_read.finish()),
                Arc::new(cache_creation.finish()),
                Arc::new(is_empty.finish()),
            ],
        )?;

        let projected = match &self.projection {
            Some(indices) => batch.project(indices)?,
            None => batch,
        };

        tracing::debug!(
            target: "gage_query::session",
            rows = projected.num_rows(),
            elapsed_ms = exec_start.elapsed().as_millis(),
            "SessionExec done",
        );

        Ok(projected)
    }
}
