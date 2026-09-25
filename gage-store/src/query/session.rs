//! The `session` table bound to the Gage store: one row per live
//! session object.
//!
//! The store's index serves everything the listing sorts, filters, and
//! counts on (id, commit, timestamps) in one query over every live
//! session. `id_prefix` is computed over the store's short-prefix set
//! of sessions, the set a session prefix resolves against first.
//! The attribute columns (`project`, `native_id`, `session_type`,
//! `summary.*`) live in each object's `attrs.json` and are read from
//! the repository only for the rows a projection needs, one `cat-file`
//! round trip each. Filters on `id` and lower bounds on `modified` are
//! applied to the index rows before any object is read.

use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{
    BooleanBuilder, Int64Builder, StringBuilder, TimestampMillisecondBuilder,
};
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
use gage_core::style::IdHighlighter;
use gage_core::uuid::short_uuid;
use gage_session::filter::{self, IdFilter};

use crate::{Order, SESSION_TYPE, SelectedTip, SessionStore, Store, StoreError};

/// Columns read from the object's `attrs.json` rather than the index.
/// A projection touching none of these never opens an object.
const ATTRS_COLS: &[&str] = &[
    "project",
    "native_mtime",
    "native_size",
    "native_id",
    "native_source",
    "session_type",
    "driver",
    "title",
    "model",
    "message_count",
    "is_empty",
    "line_count",
];

/// The commit's `modified` marker: when the store wrote the version
const MODIFIED_COL: &str = "modified";

fn stored_session_schema() -> SchemaRef {
    // Column order is shared with the driver `native_session` table:
    // identity, location, timestamps and size, provenance, content
    // summary, system. A new column joins the category it belongs to.
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // Location
        // The project the session belongs to, as the driver names it
        Field::new("project", DataType::Utf8, true),
        // Timestamps and size
        Field::new(
            MODIFIED_COL,
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        // The object's `created` marker
        Field::new(
            "created",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        // When the native artifact was last touched at its source
        Field::new(
            "native_mtime",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        // The size in bytes of the native artifact
        Field::new("native_size", DataType::Int64, false),
        // Provenance
        Field::new("native_id", DataType::Utf8, false),
        Field::new("native_source", DataType::Utf8, false),
        Field::new("session_type", DataType::Utf8, false),
        Field::new("driver", DataType::Utf8, false),
        // Content summary
        Field::new("title", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, true),
        Field::new("message_count", DataType::Int64, true),
        Field::new("is_empty", DataType::Boolean, false),
        // The number of lines in the native content, when the driver
        // reports it
        Field::new("line_count", DataType::Int64, true),
        // System
        // Short display form of the Gage id
        Field::new("id_display", DataType::Utf8, false),
        // Shortest prefix of `id` unique among every live session
        Field::new("id_prefix", DataType::Utf8, false),
        // `git:<commit sha>` of the version listed
        Field::new("locator", DataType::Utf8, false),
    ]))
}

/// The store-bound `session` table. The store is shared under a mutex
/// because its git reader is single-threaded. Store-wide, the rows
/// are every live session at its tip, served by the index; over fixed
/// versions, the rows are those versions in the order given, which is
/// how a dataset or scan scope lists its members. A fixed version
/// carries its markers, so a row costs no read until an attrs column
/// is projected.
#[derive(Debug, Clone)]
pub struct StoredSessionTable {
    store: Arc<Mutex<Store>>,
    schema: SchemaRef,
    versions: Option<Vec<SelectedTip>>,
}

impl StoredSessionTable {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            schema: stored_session_schema(),
            versions: None,
        }
    }

    /// The table over exactly `versions`, in that order. Each carries
    /// the id, commit, and markers of one session version.
    pub fn at_versions(store: Arc<Mutex<Store>>, versions: Vec<SelectedTip>) -> Self {
        Self {
            store,
            schema: stored_session_schema(),
            versions: Some(versions),
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

    /// Filters on `id` alone and lower bounds on `modified` are
    /// applied to the index rows and need no post-scan filter.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| {
                if modified_lower_bound(f).is_some() {
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
            self.versions.clone(),
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
            filters.to_vec(),
            limit,
        )))
    }
}

/// `modified >= <timestamp>` or `modified > <timestamp>` as an
/// inclusive lower bound in epoch milliseconds. `None` for any other
/// shape.
fn modified_lower_bound(expr: &Expr) -> Option<i64> {
    let Expr::BinaryExpr(BinaryExpr { left, op, right }) = expr else {
        return None;
    };
    let Expr::Column(column) = left.as_ref() else {
        return None;
    };
    if column.name != MODIFIED_COL {
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
    versions: Option<Vec<SelectedTip>>,
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
        versions: Option<Vec<SelectedTip>>,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        filters: Vec<Expr>,
        limit: Option<usize>,
    ) -> Self {
        // Only the index-served rows come newest modified first
        let equivalence = if versions.is_none() {
            modified_desc_eq_properties(&projected_schema)
        } else {
            EquivalenceProperties::new(projected_schema.clone())
        };
        let properties = PlanProperties::new(
            equivalence,
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            store,
            versions,
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
            Some(indices) => indices
                .iter()
                .any(|&i| ATTRS_COLS.contains(&self.full_schema.field(i).name().as_str())),
            None => true,
        }
    }

    fn build_batch(self) -> Result<RecordBatch> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sessions = SessionStore::from(&*store);

        // The rows to filter: every live session newest modified first,
        // or the given versions in the given order. Neither reads an
        // object.
        let all: Vec<SelectedTip> = match &self.versions {
            None => sessions
                .query()
                .order(Order::ModifiedDesc)
                .tips()
                .map_err(external)?,
            Some(versions) => versions.clone(),
        };
        // id_prefix is unique within the short-prefix set of sessions,
        // where a session prefix resolves first, plus the fixed rows
        let mut peers = store
            .short_prefix_ids(Some(SESSION_TYPE))
            .map_err(external)?;
        if self.versions.is_some() {
            for tip in &all {
                if !peers.contains(&tip.id) {
                    peers.push(tip.id.clone());
                }
            }
        }
        let prefix_len = unique_prefix_lens(peers);

        let since = self.filters.iter().filter_map(modified_lower_bound).max();
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
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut native_mtimes = TimestampMillisecondBuilder::with_capacity(len);
        let mut native_sizes = Int64Builder::with_capacity(len);
        let mut native_ids = StringBuilder::new();
        let mut native_sources = StringBuilder::new();
        let mut session_types = StringBuilder::new();
        let mut drivers = StringBuilder::new();
        let mut titles = StringBuilder::new();
        let mut models = StringBuilder::new();
        let mut message_counts = Int64Builder::with_capacity(len);
        let mut is_empties = BooleanBuilder::with_capacity(len);
        let mut line_counts = Int64Builder::with_capacity(len);
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
            // Every object carries both markers; a missing one is a
            // malformed object, not a null
            modifieds.append_value(marker(tip, "modified", tip.modified_ms)?);
            createds.append_value(marker(tip, "created", tip.created_ms)?);

            if !needs_attrs {
                native_ids.append_value("");
                native_sources.append_value("");
                session_types.append_value("");
                drivers.append_value("");
                native_mtimes.append_value(0);
                native_sizes.append_value(0);
                titles.append_null();
                models.append_null();
                message_counts.append_null();
                is_empties.append_value(false);
                line_counts.append_null();
                projects.append_null();
                continue;
            }
            let record = sessions.at_commit(&tip.sha).map_err(external)?;
            let attrs = &record.attrs;
            native_ids.append_value(&attrs.native_id);
            native_sources.append_value(&attrs.native_source);
            session_types.append_value(&attrs.session_type);
            drivers.append_value(&attrs.driver);
            native_mtimes.append_value(attrs.native_mtime);
            native_sizes.append_value(attrs.native_size as i64);
            let summary = &attrs.summary;
            titles.append_option(summary.title.as_deref());
            models.append_option(summary.model.as_deref());
            message_counts.append_option(summary.message_count.map(|v| v as i64));
            is_empties.append_value(summary.is_empty);
            line_counts.append_option(summary.line_count.map(|v| v as i64));
            projects.append_option(attrs.project.as_deref());
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(projects.finish()),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(native_mtimes.finish().with_timezone("UTC")),
                Arc::new(native_sizes.finish()),
                Arc::new(native_ids.finish()),
                Arc::new(native_sources.finish()),
                Arc::new(session_types.finish()),
                Arc::new(drivers.finish()),
                Arc::new(titles.finish()),
                Arc::new(models.finish()),
                Arc::new(message_counts.finish()),
                Arc::new(is_empties.finish()),
                Arc::new(line_counts.finish()),
                Arc::new(id_displays.finish()),
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
            ],
        )?;
        match &self.projection {
            Some(indices) => Ok(batch.project(indices)?),
            None => Ok(batch),
        }
    }
}

/// Unique prefix length of every id among its peers
fn unique_prefix_lens(ids: Vec<String>) -> HashMap<String, usize> {
    let highlighter = IdHighlighter::new(ids.clone());
    ids.into_iter()
        .map(|id| {
            let n = highlighter.unique_prefix_len(&id);
            (id, n)
        })
        .collect()
}

fn external(e: StoreError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

/// The value of a required marker of `tip`, or an error naming the
/// object when the marker is missing.
fn marker(tip: &SelectedTip, name: &str, value: Option<i64>) -> Result<i64> {
    value.ok_or_else(|| {
        external(StoreError::Parse(format!(
            "session {}: missing {name} marker",
            tip.id
        )))
    })
}

/// Advertise `[modified DESC]` when the projection keeps `modified`: the
/// index returns rows in that order, so DataFusion elides its sort and
/// pushes `LIMIT` into the scan.
fn modified_desc_eq_properties(projected_schema: &SchemaRef) -> EquivalenceProperties {
    match col(MODIFIED_COL, projected_schema) {
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
            self.versions.clone(),
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::array::StringArray;
    use datafusion::prelude::SessionContext;

    use super::StoredSessionTable;
    use crate::SessionStore;
    use crate::session::tests::{FakeDriver, fake};
    use crate::test_support::open_store;

    /// The store-bound `session` table projects one row per live
    /// session, reading `native_id` and `project` from each object's
    /// attrs.
    #[tokio::test]
    async fn session_table_projects_stored_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        {
            let sessions = SessionStore::from(&store);
            sessions
                .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
                .unwrap();
            sessions
                .add(&FakeDriver, &mut fake("s2", &[("session.jsonl", "{}\n")]))
                .unwrap();
        }

        let ctx = SessionContext::new();
        ctx.register_table(
            "session",
            Arc::new(StoredSessionTable::new(Arc::new(Mutex::new(store)))),
        )
        .unwrap();
        let batches = ctx
            .sql("SELECT native_id, project FROM session ORDER BY native_id")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        let batch = batches.first().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let native = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let project = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(native.value(0), "s1");
        assert_eq!(native.value(1), "s2");
        assert_eq!(project.value(0), "proj");
        assert_eq!(project.value(1), "proj");
    }
}
