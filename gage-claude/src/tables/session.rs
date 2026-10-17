use std::any::Any;
use std::fmt::{self, Formatter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::index::{ClaudeSessionAttrs, IndexStore};
use async_trait::async_trait;
use datafusion::arrow::array::{
    BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, StringBuilder,
    TimestampMillisecondBuilder,
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
use gage_session::SessionAttrs;

use super::cache::SessionCache;
use super::walk::{SummaryPrune, WALK_COLS, walk_sessions};
use crate::driver::{CONTENT_FORMAT, ClaudeNativeSession};
use crate::session::SessionInfo;
use gage_core::uuid::short_uuid;
use gage_session::filter;
use gage_session::system;

/// Columns whose value comes from parsing the session JSONL. Every
/// other column is filled from the directory walk. A projection that
/// touches none of these — `SELECT COUNT(*)`, `SELECT id FROM session
/// WHERE ...` — can skip per-row derivation entirely.
const SUMMARY_COLS: &[&str] = &[
    "title",
    "model",
    "message_count",
    "input_tokens",
    "output_tokens",
    "cached_tokens",
    "cost_usd",
    "is_empty",
    "files_changed",
    "files_referenced",
];

fn session_schema() -> SchemaRef {
    // Column order is shared with the store's `session` table:
    // identity, location, timestamps and size, content summary, token
    // usage, derived flags, system. A new column joins the category it
    // belongs to.
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // Location
        Field::new("project", DataType::Utf8, true),
        // Timestamps and size
        Field::new(
            "mtime",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new("size", DataType::Int64, false),
        // Provenance
        // The driver's name and version for the byte layout of `raw`;
        // a reader of `raw` checks it
        Field::new("content_format", DataType::Utf8, false),
        // Content summary
        Field::new("title", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("message_count", DataType::Int64, false),
        // Token usage, as `SessionAttrs` defines it: totals over the
        // session's API responses, each counted once
        Field::new("input_tokens", DataType::Int64, false),
        Field::new("output_tokens", DataType::Int64, false),
        Field::new("cached_tokens", DataType::Int64, false),
        Field::new("cost_usd", DataType::Float64, true),
        // Derived flags
        Field::new("is_empty", DataType::Boolean, false),
        // Files
        // Every path a tool call wrote, as the driver judged it
        Field::new("files_changed", path_list(), true),
        // Every other path a tool call's input referred to: an
        // argument named as a path, or a path-shaped token of a shell
        // command. A write the harness did not record is found here.
        Field::new("files_referenced", path_list(), true),
        // System
        // The driver's short display form of `id`
        system(Field::new("id_display", DataType::Utf8, false)),
        // The shortest prefix of `id` unique among every session in
        // the source
        system(Field::new("id_prefix", DataType::Utf8, false)),
        system(Field::new("path", DataType::Utf8, false)),
    ]))
}

fn path_list() -> DataType {
    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
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

    /// A filter over the id and the walk's own columns is applied in
    /// the walk, before any session is parsed, so it is exact and
    /// DataFusion drops its own filter and pushes the limit down.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        let cols: Vec<&str> = std::iter::once("id")
            .chain(WALK_COLS.iter().copied())
            .collect();
        Ok(filters
            .iter()
            .map(|f| filter::pushdown_cols(f, &cols))
            .collect())
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        let cache = SessionCache::shared();
        Ok(Arc::new(SessionExec::new(
            self.store.clone(),
            cache,
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
            Some(indices) => indices
                .iter()
                .any(|&i| SUMMARY_COLS.contains(&self.full_schema.field(i).name().as_str())),
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
        let outcome = walk_sessions(
            &self.store,
            &self.filters,
            "id",
            self.limit,
            true,
            SummaryPrune::NONE,
        )?;
        let sessions: Vec<SessionInfo> = outcome.sessions;

        let needs_summary = self.projection_needs_summary();
        // The Claude root the projects dir sits under, for session URLs
        let root = self
            .store
            .root()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.store.root().to_path_buf());
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
        let mut id_displays = StringBuilder::with_capacity(len, len * 8);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut projects = StringBuilder::with_capacity(len, len * 32);
        let mut paths = StringBuilder::with_capacity(len, len * 64);
        let mut mtimes = TimestampMillisecondBuilder::with_capacity(len);
        let mut sizes = Int64Builder::with_capacity(len);
        let mut content_formats = StringBuilder::with_capacity(len, len * CONTENT_FORMAT.len());
        let mut titles = StringBuilder::new();
        let mut models = StringBuilder::new();
        let mut message_counts = Int64Builder::with_capacity(len);
        let mut input_tokens = Int64Builder::with_capacity(len);
        let mut output_tokens = Int64Builder::with_capacity(len);
        let mut cached_tokens = Int64Builder::with_capacity(len);
        let mut costs = Float64Builder::with_capacity(len);
        let mut is_empty = BooleanBuilder::with_capacity(len);
        let mut files_changed = ListBuilder::new(StringBuilder::new());
        let mut files_referenced = ListBuilder::new(StringBuilder::new());

        let default_summary = ClaudeSessionAttrs {
            is_empty: true,
            ..Default::default()
        };

        for s in &sessions {
            ids.append_value(&s.id);
            id_displays.append_value(short_uuid(&s.id));
            let prefix_len = outcome
                .id_prefix_len
                .get(&s.id)
                .copied()
                .expect("walk computed a prefix for every walked id");
            id_prefixes.append_value(s.id.chars().take(prefix_len).collect::<String>());
            projects.append_value(s.project_name().as_ref());
            paths.append_value(s.src.to_string_lossy().as_ref());
            let millis = s
                .mtime
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            mtimes.append_value(millis);
            sizes.append_value(s.size as i64);
            content_formats.append_value(CONTENT_FORMAT);

            // The native session is the authority on a session's
            // attributes; the table is one consumer of it.
            let summary = if !needs_summary {
                default_summary.clone()
            } else {
                match ClaudeNativeSession::open(&s.id, &s.src, s.mtime, &root, &self.store) {
                    Ok(session) => session.claude_attrs().clone(),
                    Err(e) => {
                        tracing::warn!(session_id = %s.id, "session summary unavailable: {e}");
                        default_summary.clone()
                    }
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
            input_tokens.append_value(SessionAttrs::input_tokens(&summary).unwrap_or(0) as i64);
            output_tokens.append_value(SessionAttrs::output_tokens(&summary).unwrap_or(0) as i64);
            cached_tokens.append_value(summary.cached_tokens().unwrap_or(0) as i64);
            costs.append_option(summary.cost_usd());
            is_empty.append_value(summary.is_empty);
            append_paths(&mut files_changed, &summary.files_changed, needs_summary);
            append_paths(
                &mut files_referenced,
                &summary.files_referenced,
                needs_summary,
            );
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(projects.finish()),
                Arc::new(mtimes.finish().with_timezone("UTC")),
                Arc::new(sizes.finish()),
                Arc::new(content_formats.finish()),
                Arc::new(titles.finish()),
                Arc::new(models.finish()),
                Arc::new(message_counts.finish()),
                Arc::new(input_tokens.finish()),
                Arc::new(output_tokens.finish()),
                Arc::new(cached_tokens.finish()),
                Arc::new(costs.finish()),
                Arc::new(is_empty.finish()),
                Arc::new(files_changed.finish()),
                Arc::new(files_referenced.finish()),
                Arc::new(id_displays.finish()),
                Arc::new(id_prefixes.finish()),
                Arc::new(paths.finish()),
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

/// A path list row: null when the summary was not read, so a
/// projection without the column costs no parse
fn append_paths(builder: &mut ListBuilder<StringBuilder>, paths: &[PathBuf], known: bool) {
    if !known {
        builder.append_null();
        return;
    }
    for p in paths {
        builder.values().append_value(p.to_string_lossy());
    }
    builder.append(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The listing serves identity, location, format, and system
    /// columns without opening a transcript; every other column needs
    /// the derived attributes. A column missing from `SUMMARY_COLS`
    /// reads as its default when projected alone.
    #[test]
    fn summary_cols_name_every_derived_column() {
        let listing = ["id", "project", "mtime", "size", "content_format"];
        let mut derived: Vec<String> = session_schema()
            .fields()
            .iter()
            .filter(|f| !gage_session::is_system(f) && !listing.contains(&f.name().as_str()))
            .map(|f| f.name().to_string())
            .collect();
        let mut expected: Vec<String> = SUMMARY_COLS.iter().map(|c| c.to_string()).collect();
        derived.sort_unstable();
        expected.sort_unstable();
        assert_eq!(derived, expected);
    }
}
