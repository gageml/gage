//! The `session` table bound to the Gage store: one row per live
//! session object.
//!
//! The store's index serves everything the listing sorts, filters, and
//! counts on (id, commit, timestamps) in one query over every live
//! session, and that full id set is what `id_prefix` is computed over.
//! The attribute columns (`native_id`, `session_type`, `summary.*`)
//! live in each object's `attrs.json` and are read from the repository
//! only for the rows a projection needs, one `cat-file` round trip
//! each. Filters on `id` and lower bounds on `mtime` are applied to
//! the index rows before any object is read.

use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::compute::SortOptions;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::common::ScalarValue;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::{BinaryExpr, Operator, TableProviderFilterPushDown};
use datafusion::physical_expr::expressions::col;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalSortExpr};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::Expr;
use futures::stream;
use gage_claude::tables::filter::{self, IdFilter};
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_store::{Order, SelectedTip, SessionStore, Store, StoreError};

/// Index of the first column read from the object rather than the
/// index. A projection below this bound never opens an object.
const ATTRS_COL_START: usize = 6;

const MTIME_COL: &str = "mtime";

fn stored_session_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        // Short display form of the Gage id
        Field::new("id_display", DataType::Utf8, false),
        // Shortest prefix of `id` unique among every live session
        Field::new("id_prefix", DataType::Utf8, false),
        // `git:<commit sha>` of the version listed
        Field::new("locator", DataType::Utf8, false),
        // The commit's `modified` marker
        Field::new(
            MTIME_COL,
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new(
            "created",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new("native_id", DataType::Utf8, false),
        Field::new("native_source", DataType::Utf8, false),
        Field::new("session_type", DataType::Utf8, false),
        Field::new("driver", DataType::Utf8, false),
        Field::new("size", DataType::Int64, true),
        Field::new("title", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("message_count", DataType::Int64, true),
        // The project the session belongs to, as the driver names it
        Field::new("project", DataType::Utf8, true),
    ]))
}

/// The store-bound `session` table. The store is shared under a mutex
/// because its git reader is single-threaded.
#[derive(Debug, Clone)]
pub struct StoredSessionTable {
    store: Arc<Mutex<Store>>,
    schema: SchemaRef,
}

impl StoredSessionTable {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            schema: stored_session_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for StoredSessionTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// Filters on `id` alone and lower bounds on `mtime` are applied
    /// to the index rows and need no post-scan filter.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| {
                if mtime_lower_bound(f).is_some() {
                    TableProviderFilterPushDown::Exact
                } else {
                    filter::pushdown(f, "id")
                }
            })
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
        Ok(Arc::new(StoredSessionExec::new(
            Arc::clone(&self.store),
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
            filters.to_vec(),
            limit,
        )))
    }
}

/// `mtime >= <timestamp>` or `mtime > <timestamp>` as an inclusive
/// lower bound in epoch milliseconds. `None` for any other shape.
fn mtime_lower_bound(expr: &Expr) -> Option<i64> {
    let Expr::BinaryExpr(BinaryExpr { left, op, right }) = expr else {
        return None;
    };
    let Expr::Column(column) = left.as_ref() else {
        return None;
    };
    if column.name != MTIME_COL {
        return None;
    }
    let ms = timestamp_literal_ms(right)?;
    match op {
        Operator::GtEq => Some(ms),
        Operator::Gt => Some(ms.saturating_add(1)),
        _ => None,
    }
}

/// Epoch milliseconds of a timestamp literal, looking through a cast
/// the type coercion may have wrapped it in.
fn timestamp_literal_ms(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(value, _) => match value {
            ScalarValue::TimestampMillisecond(Some(ms), _) => Some(*ms),
            ScalarValue::TimestampSecond(Some(s), _) => s.checked_mul(1_000),
            ScalarValue::TimestampMicrosecond(Some(us), _) => Some(us.div_euclid(1_000)),
            ScalarValue::TimestampNanosecond(Some(ns), _) => Some(ns.div_euclid(1_000_000)),
            _ => None,
        },
        Expr::Cast(cast) => timestamp_literal_ms(&cast.expr),
        Expr::TryCast(cast) => timestamp_literal_ms(&cast.expr),
        _ => None,
    }
}

#[derive(Clone)]
struct StoredSessionExec {
    store: Arc<Mutex<Store>>,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for StoredSessionExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("StoredSessionExec")
            .field("limit", &self.limit)
            .field("filters", &self.filters)
            .finish()
    }
}

impl StoredSessionExec {
    fn new(
        store: Arc<Mutex<Store>>,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        filters: Vec<Expr>,
        limit: Option<usize>,
    ) -> Self {
        let properties = PlanProperties::new(
            mtime_desc_eq_properties(&projected_schema),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            store,
            full_schema,
            projected_schema,
            projection,
            filters,
            limit,
            properties,
        }
    }

    /// True when a projected column comes from the object rather than
    /// the index.
    fn projection_needs_attrs(&self) -> bool {
        match &self.projection {
            Some(indices) => indices.iter().any(|&i| i >= ATTRS_COL_START),
            None => true,
        }
    }

    fn build_batch(self) -> Result<RecordBatch> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sessions = SessionStore::from(&*store);

        // Every live session, newest modified first: the peer set for
        // id_prefix and the rows to filter
        let all = sessions
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let prefix_len = unique_prefix_lens(&all);

        let since = self.filters.iter().filter_map(mtime_lower_bound).max();
        let mut tips: Vec<SelectedTip> = all
            .into_iter()
            .filter(|t| since.is_none_or(|ms| t.modified_ms.is_some_and(|m| m >= ms)))
            .collect();
        if let Some(id_filter) = IdFilter::new(&self.filters, "id")? {
            tips = id_filter.retain(tips, |t| t.id.as_str())?;
        }
        if let Some(n) = self.limit {
            tips.truncate(n);
        }

        let needs_attrs = self.projection_needs_attrs();
        let len = tips.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut id_displays = StringBuilder::with_capacity(len, len * 8);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut mtimes = TimestampMillisecondBuilder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut native_ids = StringBuilder::new();
        let mut native_sources = StringBuilder::new();
        let mut session_types = StringBuilder::new();
        let mut drivers = StringBuilder::new();
        let mut sizes = Int64Builder::with_capacity(len);
        let mut titles = StringBuilder::new();
        let mut models = StringBuilder::new();
        let mut message_counts = Int64Builder::with_capacity(len);
        let mut projects = StringBuilder::new();

        for tip in &tips {
            ids.append_value(&tip.id);
            id_displays.append_value(short_uuid(&tip.id));
            let n = prefix_len
                .get(&tip.id)
                .copied()
                .expect("prefix set covers every selected tip");
            id_prefixes.append_value(tip.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", tip.sha));
            mtimes.append_option(tip.modified_ms);
            createds.append_option(tip.created_ms);

            if !needs_attrs {
                native_ids.append_value("");
                native_sources.append_value("");
                session_types.append_value("");
                drivers.append_value("");
                sizes.append_null();
                titles.append_null();
                models.append_null();
                message_counts.append_null();
                projects.append_null();
                continue;
            }
            let record = sessions.at_commit(&tip.sha).map_err(external)?;
            native_ids.append_value(&record.attrs.native_id);
            native_sources.append_value(&record.attrs.native_source);
            session_types.append_value(&record.attrs.session_type);
            drivers.append_value(&record.attrs.driver);
            let summary = record.attrs.summary.as_ref();
            sizes.append_option(summary.and_then(|s| s.size).map(|v| v as i64));
            titles.append_option(summary.and_then(|s| s.title.as_deref()));
            models.append_option(summary.and_then(|s| s.model.as_deref()));
            message_counts.append_option(summary.and_then(|s| s.message_count).map(|v| v as i64));
            projects.append_option(record.attrs.project.as_deref());
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(id_displays.finish()),
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
                Arc::new(mtimes.finish().with_timezone("UTC")),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(native_ids.finish()),
                Arc::new(native_sources.finish()),
                Arc::new(session_types.finish()),
                Arc::new(drivers.finish()),
                Arc::new(sizes.finish()),
                Arc::new(titles.finish()),
                Arc::new(models.finish()),
                Arc::new(message_counts.finish()),
                Arc::new(projects.finish()),
            ],
        )?;
        match &self.projection {
            Some(indices) => Ok(batch.project(indices)?),
            None => Ok(batch),
        }
    }
}

/// Unique prefix length of every id among its peers
fn unique_prefix_lens(tips: &[SelectedTip]) -> HashMap<String, usize> {
    let ids: Vec<String> = tips.iter().map(|t| t.id.clone()).collect();
    let highlighter = IdHighlighter::new(ids);
    tips.iter()
        .map(|t| (t.id.clone(), highlighter.unique_prefix_len(&t.id)))
        .collect()
}

fn external(e: StoreError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

/// Advertise `[mtime DESC]` when the projection keeps `mtime`: the
/// index returns rows in that order, so DataFusion elides its sort and
/// pushes `LIMIT` into the scan.
fn mtime_desc_eq_properties(projected_schema: &SchemaRef) -> EquivalenceProperties {
    match col(MTIME_COL, projected_schema) {
        Ok(expr) => {
            let sort = PhysicalSortExpr {
                expr,
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

impl DisplayAs for StoredSessionExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "StoredSessionExec")
    }
}

impl ExecutionPlan for StoredSessionExec {
    fn name(&self) -> &'static str {
        "StoredSessionExec"
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
        Some(Arc::new(StoredSessionExec::new(
            Arc::clone(&self.store),
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
        let stream = stream::once(async move { work.build_batch() });
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }
}
