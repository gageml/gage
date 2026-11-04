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

use std::cmp::Reverse;
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
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::{BinaryExpr, Operator, TableProviderFilterPushDown};
use datafusion::physical_expr::PhysicalExpr;
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
use gage_session::{SummaryAttrs, system};

use super::batch::readable;
use crate::{
    ExternalSession, ExternalSessionRecord, Order, SESSION_TYPE, SelectedTip, SessionStore, Store,
    StoreError,
};

/// Columns read from the object's `attrs.json` rather than the index.
/// A projection touching none of these never opens an object.
const ATTRS_COLS: &[&str] = &[
    "title",
    "project",
    "session_type",
    "driver",
    "content_format",
    "size",
    "origin",
    "summary",
];

/// The commit's `modified` marker: when the store wrote the version
const MODIFIED_COL: &str = "modified";

fn stored_session_schema() -> SchemaRef {
    // Column order is shared with the driver `native_session` table:
    // identity, location, timestamps, provenance, content, summary,
    // system. A new column joins the category it belongs to.
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // The session's title, as the driver derives it; also carried
        // in `summary`
        Field::new("title", DataType::Utf8, true),
        // Location
        // The project the session belongs to, as the driver names it
        Field::new("project", DataType::Utf8, true),
        // Timestamps
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
        // Provenance
        Field::new("session_type", DataType::Utf8, false),
        Field::new("driver", DataType::Utf8, false),
        // The driver's name and version for the byte layout of the
        // stored content; a reader of `raw` checks it
        Field::new("content_format", DataType::Utf8, false),
        // The total byte size of the stored content under `files.d`
        Field::new("size", DataType::Int64, false),
        // The session this one was made from, as a URL
        Field::new("origin", DataType::Utf8, false),
        // The driver's projection of the session, as a JSON object:
        // title, model, message_count, is_empty, line_count, the token
        // totals, and cost_usd when the driver reports them
        Field::new("summary", DataType::Utf8, true),
        // System
        // Short display form of the Gage id
        system(Field::new("id_display", DataType::Utf8, false)),
        // Shortest prefix of `id` unique among every live session
        system(Field::new("id_prefix", DataType::Utf8, false)),
        // `git:<commit sha>` of the version listed
        system(Field::new("locator", DataType::Utf8, false)),
    ]))
}

/// The store-bound `session` table. The store is shared under a mutex
/// because its git reader is single-threaded. Store-wide, the rows
/// are every live session at its tip, served by the index; over fixed
/// versions, the rows are those versions in the order given, which is
/// how a dataset or scan scope lists its members. A fixed version
/// carries its markers, so a row costs no read until an attrs column
/// is projected. Over external sessions, the rows are the session
/// directories given, each read from the file system on each scan.
#[derive(Debug, Clone)]
pub struct StoredSessionTable {
    store: Arc<Mutex<Store>>,
    schema: SchemaRef,
    rows: Rows,
}

/// Which sessions the table serves.
#[derive(Debug, Clone)]
enum Rows {
    /// Every live session at its tip
    Live,
    /// Exactly these versions, in this order
    Versions(Vec<SelectedTip>),
    /// Exactly these session directories, newest modified first
    External(Vec<ExternalSession>),
}

impl StoredSessionTable {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            schema: stored_session_schema(),
            rows: Rows::Live,
        }
    }

    /// The table over exactly `versions`, in that order. Each carries
    /// the id, commit, and markers of one session version.
    pub fn at_versions(store: Arc<Mutex<Store>>, versions: Vec<SelectedTip>) -> Self {
        Self {
            store,
            schema: stored_session_schema(),
            rows: Rows::Versions(versions),
        }
    }

    /// The table over `sessions`, session directories read on each
    /// scan, newest modified first. An external session has no
    /// commit: its `locator` is its directory, and its `modified` and
    /// `created` are the mtime of its `attrs.json`.
    pub fn external(store: Arc<Mutex<Store>>, sessions: Vec<ExternalSession>) -> Self {
        Self {
            store,
            schema: stored_session_schema(),
            rows: Rows::External(sessions),
        }
    }
}

#[async_trait]
impl TableProvider for StoredSessionTable {
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
            self.rows.clone(),
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
    rows: Rows,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    limit: Option<usize>,
    properties: Arc<PlanProperties>,
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
        rows: Rows,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        filters: Vec<Expr>,
        limit: Option<usize>,
    ) -> Self {
        // Only the index-served rows come newest modified first
        let equivalence = if matches!(rows, Rows::Live) {
            modified_desc_eq_properties(&projected_schema)
        } else {
            EquivalenceProperties::new(projected_schema.clone())
        };
        let properties = Arc::new(PlanProperties::new(
            equivalence,
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
        Self {
            store,
            rows,
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
        // the given versions in the given order, or the external
        // sessions newest modified first. None reads an object.
        let (all, mut externals): (Vec<SelectedTip>, Vec<ExternalSessionRecord>) = match &self.rows
        {
            Rows::Live => (
                sessions
                    .query()
                    .order(Order::ModifiedDesc)
                    .tips()
                    .map_err(external)?,
                Vec::new(),
            ),
            Rows::Versions(versions) => (versions.clone(), Vec::new()),
            Rows::External(sessions) => {
                let mut records = sessions
                    .iter()
                    .map(ExternalSession::read)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(external)?;
                records.sort_by_key(|r| Reverse(r.attrs_modified_ms));
                (Vec::new(), records)
            }
        };
        // id_prefix is unique within the set a prefix resolves against:
        // the short-prefix set of sessions plus the fixed rows, or the
        // external sessions alone
        let peers = match &self.rows {
            Rows::External(_) => externals.iter().map(|r| r.id.clone()).collect(),
            Rows::Live | Rows::Versions(_) => {
                let mut peers = store
                    .short_prefix_ids(Some(SESSION_TYPE))
                    .map_err(external)?;
                for id in all.iter().map(|t| &t.id) {
                    if !peers.contains(id) {
                        peers.push(id.clone());
                    }
                }
                peers
            }
        };
        let prefix_len = unique_prefix_lens(peers);

        let since = self.filters.iter().filter_map(modified_lower_bound).max();
        let mut tips: Vec<SelectedTip> = all
            .into_iter()
            .filter(|t| since.is_none_or(|ms| t.modified_ms.is_some_and(|m| m >= ms)))
            .collect();
        externals.retain(|r| since.is_none_or(|ms| r.attrs_modified_ms >= ms));
        let id_filter = IdFilter::new(&self.filters, "id")?;
        if let Some(f) = &id_filter {
            tips = f.retain(tips, |t| t.id.as_str())?;
            externals = f.retain(externals, |r| r.id.as_str())?;
        }
        if let Some(n) = self.limit {
            tips.truncate(n);
            externals.truncate(n);
        }

        let needs_attrs = self.projection_needs_attrs();
        let len = tips.len() + externals.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut titles = StringBuilder::new();
        let mut id_displays = StringBuilder::with_capacity(len, len * 8);
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut session_types = StringBuilder::new();
        let mut drivers = StringBuilder::new();
        let mut content_formats = StringBuilder::new();
        let mut sizes = Int64Builder::with_capacity(len);
        let mut origins = StringBuilder::new();
        let mut summaries = StringBuilder::new();
        let mut projects = StringBuilder::new();

        for tip in &tips {
            // Read before any column is appended, so a skipped object
            // leaves every column the same length
            let record = if needs_attrs {
                match readable(sessions.at_commit(&tip.sha))? {
                    Some(record) => Some(record),
                    None => continue,
                }
            } else {
                None
            };
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

            let Some(record) = record else {
                titles.append_null();
                session_types.append_value("");
                drivers.append_value("");
                content_formats.append_value("");
                sizes.append_value(0);
                origins.append_value("");
                summaries.append_null();
                projects.append_null();
                continue;
            };
            let attrs = &record.attrs;
            titles.append_option(attrs.summary.title.as_deref());
            session_types.append_value(&attrs.session_type);
            drivers.append_value(&attrs.driver);
            content_formats.append_value(&attrs.content_format);
            sizes.append_value(attrs.size as i64);
            origins.append_value(&attrs.origin);
            summaries.append_value(summary_json(&attrs.summary)?);
            projects.append_option(attrs.project.as_deref());
        }

        // An external session has no commit: its id prefix is unique
        // among the external sessions, its locator is its directory,
        // and its created is its modified
        for r in &externals {
            ids.append_value(&r.id);
            id_displays.append_value(short_uuid(&r.id));
            let n = prefix_len
                .get(&r.id)
                .copied()
                .expect("prefix set covers every external session");
            id_prefixes.append_value(r.id.chars().take(n).collect::<String>());
            locators.append_value(r.dir.display().to_string());
            modifieds.append_value(r.attrs_modified_ms);
            createds.append_value(r.attrs_modified_ms);
            titles.append_option(r.attrs.summary.title.as_deref());
            session_types.append_value(&r.attrs.session_type);
            drivers.append_value(&r.attrs.driver);
            content_formats.append_value(&r.attrs.content_format);
            sizes.append_value(r.attrs.size as i64);
            origins.append_value(&r.attrs.origin);
            summaries.append_value(summary_json(&r.attrs.summary)?);
            projects.append_option(r.attrs.project.as_deref());
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(titles.finish()),
                Arc::new(projects.finish()),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(session_types.finish()),
                Arc::new(drivers.finish()),
                Arc::new(content_formats.finish()),
                Arc::new(sizes.finish()),
                Arc::new(origins.finish()),
                Arc::new(summaries.finish()),
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

/// The driver's session summary as a JSON object string, the value the
/// `summary` column carries.
fn summary_json(summary: &SummaryAttrs) -> Result<String> {
    summary
        .to_json()
        .map_err(|e| external(StoreError::Parse(format!("session summary encode: {e}"))))
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
        Some(Arc::new(StoredSessionExec::new(
            Arc::clone(&self.store),
            self.rows.clone(),
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

    use datafusion::arrow::array::{Array, StringArray};
    use datafusion::prelude::SessionContext;

    use gage_core::uuid::new_uuid;

    use super::{StoredSessionTable, unique_prefix_lens};
    use crate::session::tests::{FakeDriver, fake, write_session_dir};
    use crate::test_support::open_store;
    use crate::{ExternalSession, SessionAttrsRecord, SessionDirLayout, SessionStore};

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
            .sql("SELECT origin, project FROM session ORDER BY origin")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        let batch = batches.first().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let origin = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let project = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(origin.value(0), "fake:s1");
        assert_eq!(origin.value(1), "fake:s2");
        assert_eq!(project.value(0), "proj");
        assert_eq!(project.value(1), "proj");
    }

    /// The `summary` column carries the driver's projection as a JSON
    /// object, with the token totals and cost present only where the
    /// driver reported them. `title` is a typed column beside it and
    /// stays in the JSON.
    #[tokio::test]
    async fn session_table_projects_summary_json() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        {
            let sessions = SessionStore::from(&store);
            let mut with = fake("s1", &[("session.jsonl", "{}\n")]);
            with.summary.title = Some("Fix the parser".to_string());
            with.summary.tokens = Some((1200, 300, 1000));
            with.summary.cost_usd = Some(0.5);
            sessions.add(&FakeDriver, &mut with).unwrap();
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
            .sql("SELECT origin, summary, title FROM session ORDER BY origin")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let batch = batches.first().unwrap();
        let summary = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let title = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(title.value(0), "Fix the parser");
        assert!(title.is_null(1));
        let with: serde_json::Value = serde_json::from_str(summary.value(0)).unwrap();
        assert_eq!(with["title"], "Fix the parser");
        assert_eq!(with["input_tokens"], 1200);
        assert_eq!(with["output_tokens"], 300);
        assert_eq!(with["cached_tokens"], 1000);
        assert_eq!(with["cost_usd"], 0.5);
        let without: serde_json::Value = serde_json::from_str(summary.value(1)).unwrap();
        assert!(without.get("input_tokens").is_none());
        assert!(without.get("cost_usd").is_none());
    }

    async fn ids(ctx: &SessionContext, sql: &str) -> Vec<String> {
        let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
        let mut out = Vec::new();
        for batch in &batches {
            let col = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            out.extend((0..batch.num_rows()).map(|i| col.value(i).to_string()));
        }
        out
    }

    /// The external table lists the session directories it was given,
    /// newest modified first, with an id prefix unique among them
    /// alone, as a publish command resolves it; it lists no stored
    /// session, and the store-wide table lists no external session.
    /// Both respect an id filter.
    #[tokio::test]
    async fn external_table_lists_the_given_session_directories_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = fake("orig", &[("session.jsonl", "hello\n")]);
        let outcome = SessionStore::from(&store)
            .add(&FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let attrs = SessionAttrsRecord {
            origin: format!("session:{}@{}", original.id, original.commit_sha),
            ..original.attrs.clone()
        };
        let external_root = tmp.path().join("external");
        let (first, second) = (new_uuid(), new_uuid());
        let first_dir = write_session_dir(&external_root, &first, &attrs, "[]\n", &[]);
        let second_dir = write_session_dir(&external_root, &second, &attrs, "[]\n", &[]);
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(90);
        std::fs::File::options()
            .write(true)
            .open(SessionDirLayout::new(&second_dir).attrs_file())
            .unwrap()
            .set_modified(later)
            .unwrap();
        let externals = vec![
            ExternalSession {
                id: first.clone(),
                dir: first_dir.clone(),
            },
            ExternalSession {
                id: second.clone(),
                dir: second_dir,
            },
        ];

        let store = Arc::new(Mutex::new(store));
        let ctx = SessionContext::new();
        ctx.register_table(
            "session",
            Arc::new(StoredSessionTable::new(Arc::clone(&store))),
        )
        .unwrap();
        ctx.register_table(
            "external",
            Arc::new(StoredSessionTable::external(store, externals)),
        )
        .unwrap();

        let stored = ids(&ctx, "SELECT id FROM session").await;
        assert_eq!(stored, vec![original.id.clone()]);

        let listed = ids(&ctx, "SELECT id FROM external").await;
        assert_eq!(listed, vec![second.clone(), first.clone()]);

        let one = ids(
            &ctx,
            &format!("SELECT id FROM external WHERE id = '{first}'"),
        )
        .await;
        assert_eq!(one, vec![first.clone()]);

        let prefixes = ids(
            &ctx,
            "SELECT id_prefix FROM external ORDER BY modified DESC",
        )
        .await;
        let among_externals = unique_prefix_lens(vec![first.clone(), second.clone()]);
        let expect = |id: &str| id.chars().take(among_externals[id]).collect::<String>();
        assert_eq!(prefixes, [expect(&second), expect(&first)]);
        assert_ne!(prefixes[0], prefixes[1]);

        let locator = ids(
            &ctx,
            &format!("SELECT locator FROM external WHERE id = '{first}'"),
        )
        .await;
        assert_eq!(locator, vec![first_dir.display().to_string()]);
    }
}
