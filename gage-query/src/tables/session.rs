use std::any::Any;
use std::fmt::{self, Formatter};
use std::path::PathBuf;
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
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use gage_claude::session::{SessionInfo, SessionListBuilder};
use gage_index::{IndexStore, SessionAggregates};

use super::store_scan::reconcile_for_query;
use crate::filter::{self, IdFilter};

// Column layout in the merged `session` schema. Cheap columns
// (filled from the directory walk) come first; expensive columns
// (read from the store's consolidated aggregates) come after,
// starting at `EXPENSIVE_START`.
const EXPENSIVE_START: usize = 5; // title is the first expensive column
const NUM_COLS: usize = 13;

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
        let support = filters.iter().map(|f| filter::pushdown(f, "id")).collect();
        Ok(support)
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        reconcile_for_query(&self.store).await?;
        let id_filter = IdFilter::new(filters, "id")?;
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        if tracing::enabled!(tracing::Level::DEBUG) {
            let cols: Vec<&str> = match projection {
                Some(indices) => indices
                    .iter()
                    .map(|&i| self.schema.field(i).name().as_str())
                    .collect(),
                None => self
                    .schema
                    .fields()
                    .iter()
                    .map(|f| f.name().as_str())
                    .collect(),
            };
            tracing::debug!(
                target: "gage_query::session",
                projection = ?cols,
                filters = filters.len(),
                id_filter = ?id_filter,
                limit_pushdown = ?limit,
                "session::scan invoked",
            );
        }
        Ok(Arc::new(SessionExec::new(
            self.schema.clone(),
            projected_schema,
            self.store.clone(),
            projection.cloned(),
            id_filter,
            limit,
        )))
    }
}

#[derive(Debug, Clone)]
struct SessionExec {
    store: Arc<IndexStore>,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    id_filter: Option<IdFilter>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl SessionExec {
    fn new(
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        store: Arc<IndexStore>,
        projection: Option<Vec<usize>>,
        id_filter: Option<IdFilter>,
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
            full_schema,
            projected_schema,
            projection,
            id_filter,
            limit,
            properties,
        }
    }

    /// True if the requested projection only touches cheap columns
    /// (those filled from the directory walk) — lets us skip loading
    /// the aggregates file.
    fn projection_is_cheap_only(&self) -> bool {
        match &self.projection {
            Some(indices) => indices.iter().all(|&i| i < EXPENSIVE_START),
            None => false,
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

    /// Report the current fetch (limit) so DataFusion's LimitPushdown
    /// rule knows what's already plumbed through.
    fn fetch(&self) -> Option<usize> {
        self.limit
    }

    /// Accept a fetch from DataFusion's LimitPushdown. Combined with
    /// our advertised `[mtime DESC]` ordering, this is what lets
    /// `ORDER BY mtime DESC LIMIT N` open at most N session files.
    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        Some(Arc::new(SessionExec::new(
            self.full_schema.clone(),
            self.projected_schema.clone(),
            self.store.clone(),
            self.projection.clone(),
            self.id_filter.clone(),
            limit,
        )))
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let exec_start = std::time::Instant::now();
        let mut builder = SessionListBuilder::new().root(self.store.root());
        // Limit pushdown is only safe when there is no extra
        // post-filter that could reject rows. An `id` filter is applied
        // here per row, so limit pushdown is skipped whenever one is set.
        let walk_limit_applied = if self.id_filter.is_none()
            && let Some(n) = self.limit
        {
            builder = builder.limit(n);
            Some(n)
        } else {
            None
        };
        let walk_start = std::time::Instant::now();
        let sessions = builder.build();
        let walk_ms = walk_start.elapsed().as_millis();
        let walked = sessions.len();

        let cheap_only = self.projection_is_cheap_only();

        let sessions: Vec<SessionInfo> = match &self.id_filter {
            Some(f) => f.retain(sessions, |s| s.id.as_str())?,
            None => sessions.into_iter().collect(),
        };
        let files: Vec<(String, String, PathBuf, std::time::SystemTime, u64)> = sessions
            .into_iter()
            .map(|s| {
                let project = s.project_name().into_owned();
                (s.id, project, s.src, s.mtime, s.size)
            })
            .collect();

        tracing::debug!(
            target: "gage_query::session",
            walked,
            after_id_filter = files.len(),
            walk_ms,
            walk_limit_applied = ?walk_limit_applied,
            path = if cheap_only { "cheap_only" } else { "aggregates" },
            "SessionExec walk complete",
        );

        // The expensive columns come from the store's consolidated
        // aggregates — written by the reconcile pass that ran at scan
        // time. A session absent from the map (added during a
        // skipped, lock-contended reconcile) gets default aggregates
        // until the next pass absorbs it.
        let aggregates = if cheap_only {
            std::collections::HashMap::new()
        } else {
            self.store.load_aggregates().unwrap_or_else(|e| {
                tracing::warn!("unreadable session aggregates: {e}");
                std::collections::HashMap::new()
            })
        };
        let default_aggregates = SessionAggregates {
            is_empty: true,
            ..Default::default()
        };

        let len = files.len();
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

        for (id, project, path, mtime, size) in &files {
            ids.append_value(id);
            projects.append_value(project);
            paths.append_value(path.to_string_lossy().as_ref());
            let millis = mtime
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            mtimes.append_value(millis);
            sizes.append_value(*size as i64);

            let row = if cheap_only {
                &default_aggregates
            } else {
                aggregates.get(id).unwrap_or(&default_aggregates)
            };
            match &row.title {
                Some(t) => titles.append_value(t),
                None => titles.append_null(),
            }
            match &row.model {
                Some(m) => models.append_value(m),
                None => models.append_null(),
            }
            message_counts.append_value(row.message_count);
            input_tokens.append_value(row.input_tokens);
            output_tokens.append_value(row.output_tokens);
            cache_read.append_value(row.cache_read_input_tokens);
            cache_creation.append_value(row.cache_creation_input_tokens);
            is_empty.append_value(row.is_empty);
        }

        let mut columns: Vec<Arc<dyn datafusion::arrow::array::Array>> =
            Vec::with_capacity(NUM_COLS);
        columns.push(Arc::new(ids.finish()));
        columns.push(Arc::new(projects.finish()));
        columns.push(Arc::new(paths.finish()));
        columns.push(Arc::new(mtimes.finish().with_timezone("UTC")));
        columns.push(Arc::new(sizes.finish()));
        columns.push(Arc::new(titles.finish()));
        columns.push(Arc::new(models.finish()));
        columns.push(Arc::new(message_counts.finish()));
        columns.push(Arc::new(input_tokens.finish()));
        columns.push(Arc::new(output_tokens.finish()));
        columns.push(Arc::new(cache_read.finish()));
        columns.push(Arc::new(cache_creation.finish()));
        columns.push(Arc::new(is_empty.finish()));

        let batch = RecordBatch::try_new(self.full_schema.clone(), columns)?;

        tracing::debug!(
            target: "gage_query::session",
            rows = batch.num_rows(),
            elapsed_ms = exec_start.elapsed().as_millis(),
            "SessionExec done",
        );

        Ok(Box::pin(MemoryStream::try_new(
            vec![batch],
            self.projected_schema.clone(),
            self.projection.clone(),
        )?))
    }
}
