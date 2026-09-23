//! The `note` table bound to the Gage store: one row per live note
//! object.
//!
//! The index serves `id`, `created`, `modified`, `id_prefix`, and
//! `locator` for every live note in one query. Every other column
//! lives in the object and is read only for the rows a projection
//! needs, one `cat-file` round trip each. Filters on `id` and `name`
//! equality are applied before any object is read: `id` against the
//! index rows, `name` through the index's attribute table.

use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::common::ScalarValue;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::{BinaryExpr, Operator, TableProviderFilterPushDown};
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::Expr;
use futures::stream;
use gage_core::style::IdHighlighter;
use gage_session::filter::{self, IdFilter};

use crate::{NOTE_TYPE, NoteStore, NoteValue, Order, SelectedTip, Store, StoreError};

/// Columns read from the object rather than the index. A projection
/// touching none of these never opens an object.
const ATTRS_COLS: &[&str] = &[
    "name", "target", "author", "value", "text", "metadata", "scan",
];

const NAME_COL: &str = "name";

fn stored_note_schema() -> SchemaRef {
    // Column order: identity, key, value, timestamps, provenance,
    // system. A new column joins the category it belongs to.
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // Key
        Field::new(NAME_COL, DataType::Utf8, false),
        // Gage URL of the target object, with any line fragment
        Field::new("target", DataType::Utf8, true),
        // Gage URL of the writer
        Field::new("author", DataType::Utf8, false),
        // Value
        // The note value as JSON text: `value.json` as stored, or
        // `value.txt` encoded as a JSON string
        Field::new("value", DataType::Utf8, false),
        // `value.txt` as stored; null for a note with `value.json`
        Field::new("text", DataType::Utf8, true),
        // Writer-defined JSON object, as stored
        Field::new("metadata", DataType::Utf8, true),
        // Timestamps
        Field::new(
            "created",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new(
            "modified",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        // Provenance
        // The scan the note was written during
        Field::new("scan", DataType::Utf8, true),
        // System
        // Shortest prefix of `id` unique among every live note
        Field::new("id_prefix", DataType::Utf8, false),
        // `git:<commit sha>` of the version listed
        Field::new("locator", DataType::Utf8, false),
    ]))
}

/// The store-bound `note` table. The store is shared under a mutex
/// because its git reader is single-threaded.
#[derive(Debug, Clone)]
pub struct StoredNoteTable {
    store: Arc<Mutex<Store>>,
    schema: SchemaRef,
}

impl StoredNoteTable {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            schema: stored_note_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for StoredNoteTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// Filters on `id` alone and `name = <literal>` are applied before
    /// any object is read and need no post-scan filter.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| {
                if name_equals(f).is_some() {
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
        Ok(Arc::new(StoredNoteExec::new(
            Arc::clone(&self.store),
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
            filters.to_vec(),
            limit,
        )))
    }
}

/// The literal of a `name = <literal>` or `<literal> = name` filter.
/// `None` for any other shape.
fn name_equals(expr: &Expr) -> Option<String> {
    let Expr::BinaryExpr(BinaryExpr { left, op, right }) = expr else {
        return None;
    };
    if *op != Operator::Eq {
        return None;
    }
    match (left.as_ref(), right.as_ref()) {
        (Expr::Column(column), Expr::Literal(value, _))
        | (Expr::Literal(value, _), Expr::Column(column))
            if column.name == NAME_COL =>
        {
            string_literal(value)
        }
        _ => None,
    }
}

fn string_literal(value: &ScalarValue) -> Option<String> {
    match value {
        ScalarValue::Utf8(Some(s))
        | ScalarValue::LargeUtf8(Some(s))
        | ScalarValue::Utf8View(Some(s)) => Some(s.clone()),
        _ => None,
    }
}

#[derive(Clone)]
struct StoredNoteExec {
    store: Arc<Mutex<Store>>,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for StoredNoteExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("StoredNoteExec")
            .field("limit", &self.limit)
            .field("filters", &self.filters)
            .finish()
    }
}

impl StoredNoteExec {
    fn new(
        store: Arc<Mutex<Store>>,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        filters: Vec<Expr>,
        limit: Option<usize>,
    ) -> Self {
        let properties = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
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
        let notes = NoteStore::from(&*store);

        // Name equality goes to the index's attribute table; every
        // other selection happens on the returned tips
        let mut query = notes.query().order(Order::CreatedDesc);
        for name in self.filters.iter().filter_map(name_equals) {
            query = query.name(&name);
        }
        let mut tips: Vec<SelectedTip> = query.tips().map_err(external)?;
        if let Some(id_filter) = IdFilter::new(&self.filters, "id")? {
            tips = id_filter.retain(tips, |t| t.id.as_str())?;
        }
        if let Some(n) = self.limit {
            tips.truncate(n);
        }
        // id_prefix is unique among every live note
        let peers = store.short_prefix_ids(Some(NOTE_TYPE)).map_err(external)?;
        let prefix_len = unique_prefix_lens(peers);

        let needs_attrs = self.projection_needs_attrs();
        let len = tips.len();
        let mut ids = StringBuilder::with_capacity(len, len * 26);
        let mut names = StringBuilder::new();
        let mut targets = StringBuilder::new();
        let mut authors = StringBuilder::new();
        let mut values = StringBuilder::new();
        let mut texts = StringBuilder::new();
        let mut metadatas = StringBuilder::new();
        let mut createds = TimestampMillisecondBuilder::with_capacity(len);
        let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
        let mut scans = StringBuilder::new();
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);

        for tip in &tips {
            ids.append_value(&tip.id);
            createds.append_option(tip.created_ms);
            modifieds.append_option(tip.modified_ms);
            let n = prefix_len
                .get(&tip.id)
                .copied()
                .expect("prefix set covers every selected tip");
            id_prefixes.append_value(tip.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", tip.sha));

            if !needs_attrs {
                names.append_value("");
                targets.append_null();
                authors.append_value("");
                values.append_value("");
                texts.append_null();
                metadatas.append_null();
                scans.append_null();
                continue;
            }
            let note = notes.at_commit(&tip.sha).map_err(external)?;
            names.append_value(&note.name);
            targets.append_option(note.target.as_deref());
            authors.append_value(&note.author);
            let (value, text) = match &note.value {
                NoteValue::Text(t) => (serde_json::Value::String(t.clone()).to_string(), Some(t)),
                NoteValue::Json(v) => (v.to_string(), None),
            };
            values.append_value(value);
            texts.append_option(text.map(String::as_str));
            metadatas.append_option(note.metadata.as_ref().map(|m| m.to_string()));
            scans.append_option(note.scan.as_deref());
        }

        let batch = RecordBatch::try_new(
            self.full_schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(names.finish()),
                Arc::new(targets.finish()),
                Arc::new(authors.finish()),
                Arc::new(values.finish()),
                Arc::new(texts.finish()),
                Arc::new(metadatas.finish()),
                Arc::new(createds.finish().with_timezone("UTC")),
                Arc::new(modifieds.finish().with_timezone("UTC")),
                Arc::new(scans.finish()),
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

impl DisplayAs for StoredNoteExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "StoredNoteExec")
    }
}

impl ExecutionPlan for StoredNoteExec {
    fn name(&self) -> &'static str {
        "StoredNoteExec"
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
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::prelude::SessionContext;

    use super::StoredNoteTable;
    use crate::test_support::open_store;
    use crate::{NoteInput, NoteStore, NoteValue};

    fn column(batch: &RecordBatch, i: usize) -> Vec<Option<String>> {
        let col = batch
            .column(i)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        (0..batch.num_rows())
            .map(|r| col.is_valid(r).then(|| col.value(r).to_string()))
            .collect()
    }

    /// `value` is JSON text for both storage forms, `text` is the
    /// plain-text form alone, and `metadata` is the stored object.
    /// A `name` equality is served by the index.
    #[tokio::test]
    async fn note_table_projects_stored_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        {
            let notes = NoteStore::from(&store);
            notes
                .create(NoteInput {
                    name: "summary",
                    value: NoteValue::Text("prose".into()),
                    author: "user:test",
                    target: None,
                    metadata: Some(serde_json::json!({"model": "m"})),
                })
                .unwrap();
            notes
                .create(NoteInput {
                    name: "rating",
                    value: NoteValue::Json(serde_json::json!({"score": 3})),
                    author: "user:test",
                    target: None,
                    metadata: None,
                })
                .unwrap();
        }

        let ctx = SessionContext::new();
        ctx.register_table(
            "note",
            Arc::new(StoredNoteTable::new(Arc::new(Mutex::new(store)))),
        )
        .unwrap();

        let batches = ctx
            .sql("SELECT name, value, text, metadata FROM note ORDER BY name")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let batch = batches.first().unwrap();
        assert_eq!(
            column(batch, 0),
            [Some("rating".into()), Some("summary".into())]
        );
        assert_eq!(
            column(batch, 1),
            [Some(r#"{"score":3}"#.into()), Some(r#""prose""#.into())]
        );
        assert_eq!(column(batch, 2), [None, Some("prose".into())]);
        assert_eq!(column(batch, 3), [None, Some(r#"{"model":"m"}"#.into())]);

        let batches = ctx
            .sql("SELECT id_prefix, locator FROM note WHERE name = 'rating'")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let batch = batches.first().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert!(column(batch, 1)[0].as_deref().unwrap().starts_with("git:"));
    }
}
