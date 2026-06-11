use std::any::Any;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::array::{Array, BooleanArray, StringArray};
use datafusion::arrow::compute::filter_record_batch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::common::DFSchema;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::ExecutionProps;
use datafusion::logical_expr::expr_fn::in_list;
use datafusion::physical_expr::{PhysicalExpr, create_physical_expr};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_plan::filter::FilterExec;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::prelude::*;
use gage_index::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_RAW, COL_SESSION_ID, COL_SUBTYPE, COL_TEXT,
    COL_TIMESTAMP, COL_TYPE, COL_UUID, IndexStore,
};

use super::SessionSource;
use super::store_scan::{
    reconcile_for_query, references_text_search, scan_store_files, store_files,
    text_search_queries, unqualify,
};
use crate::filter::{self, IdFilter};

/// The store columns serving the `message` table, in table-column
/// order. The table is a filter over the store —
/// `type IN ('user','assistant') AND text IS NOT NULL` — since the
/// derivation leaves `text` non-null (possibly empty) exactly for
/// message rows.
const STORE_PROJECTION: &[usize] = &[
    COL_SESSION_ID,
    COL_LINE,
    COL_UUID,
    COL_TYPE,
    COL_SUBTYPE,
    COL_TEXT,
    COL_TIMESTAMP,
    COL_ATTACHMENTS,
    COL_IDE_TAGS,
    COL_RAW,
];

/// Cap on `line IN (...)` pushdown. Above this, matched coordinates
/// still prune files but row groups scan unpruned — a huge IN list
/// costs more in planning than it saves in decode.
const LINE_PUSHDOWN_CAP: usize = 1000;

static MESSAGE_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(
        gage_index::store_schema()
            .project(STORE_PROJECTION)
            .expect("store schema serves message projection"),
    )
});

fn message_schema() -> SchemaRef {
    MESSAGE_SCHEMA.clone()
}

/// The exact row condition distinguishing message rows in the store.
fn message_row_filter() -> Expr {
    in_list(col("type"), vec![lit("user"), lit("assistant")], false)
        .and(col("text").is_not_null())
}

#[derive(Debug, Clone)]
pub struct MessageTable {
    source: SessionSource,
    schema: SchemaRef,
}

impl MessageTable {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self {
            source: SessionSource::Store(store),
            schema: message_schema(),
        }
    }

    /// Build a `MessageTable` scoped to one session file.
    pub fn with_session(session_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            source: SessionSource::SingleSession {
                session_id: session_id.into(),
                path: path.into(),
            },
            schema: message_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for MessageTable {
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
        // `session_id`-only filters are Exact (applied by file
        // pruning). Everything else — including `text_search`
        // conjuncts — is Inexact: DataFusion re-applies the predicate
        // row-wise over the scanned rows, which is what makes index
        // staleness one-directional (a stale index can omit rows but
        // never wrongly include one).
        let support = filters
            .iter()
            .map(|f| filter::pushdown(f, "session_id"))
            .collect();
        Ok(support)
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match &self.source {
            SessionSource::Store(store) => {
                self.scan_store(store, state, projection, filters).await
            }
            SessionSource::SingleSession { session_id, path } => {
                let keep = match IdFilter::new(filters, "session_id")? {
                    Some(f) => f.matches(session_id)?,
                    None => true,
                };
                let batch = if keep {
                    derive_message_batch(session_id, path).await?
                } else {
                    RecordBatch::new_empty(self.schema.clone())
                };
                let mem = MemTable::try_new(self.schema.clone(), vec![vec![batch]])?;
                mem.scan(state, projection, &[], limit).await
            }
        }
    }
}

impl MessageTable {
    async fn scan_store(
        &self,
        store: &Arc<IndexStore>,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
    ) -> Result<Arc<dyn ExecutionPlan>> {
        reconcile_for_query(store).await?;
        let mut files = store_files(store, filters, "session_id")?;

        // Index acceleration: `text_search(text, '<literal>')`
        // conjuncts prune the scan to matched coordinates. The
        // pushdown is Inexact, so DataFusion re-applies the predicate
        // over whatever we return — false positives are filtered, and
        // only omissions (bounded staleness) are possible.
        let queries = text_search_queries(filters);
        let mut line_filter: Option<Expr> = None;
        if !queries.is_empty() {
            let coords = search_coordinates(store, queries).await?;
            let matched: HashSet<&str> = coords.iter().map(|(id, _)| id.as_str()).collect();
            files.retain(|(id, _)| matched.contains(id.as_str()));
            if coords.len() <= LINE_PUSHDOWN_CAP {
                let lines: HashSet<i64> = coords.iter().map(|(_, line)| *line).collect();
                let mut lines: Vec<i64> = lines.into_iter().collect();
                lines.sort_unstable();
                line_filter = Some(in_list(
                    col("line"),
                    lines.into_iter().map(lit).collect(),
                    false,
                ));
            }
        }

        // Inner scan: the requested columns plus whatever the exact
        // filter below needs.
        let table_projection: Vec<usize> = match projection {
            Some(indices) => indices.clone(),
            None => (0..self.schema.fields().len()).collect(),
        };
        let mut store_projection: Vec<usize> = table_projection
            .iter()
            .filter_map(|&i| STORE_PROJECTION.get(i).copied())
            .collect();
        let requested = store_projection.len();
        let mut extras = vec![COL_TYPE, COL_TEXT];
        if line_filter.is_some() {
            extras.push(COL_LINE);
        }
        for extra in extras {
            if !store_projection.contains(&extra) {
                store_projection.push(extra);
            }
        }

        let forwarded: Vec<Expr> = filters
            .iter()
            .filter(|f| !references_text_search(f))
            .map(unqualify)
            .collect();
        let paths: Vec<PathBuf> = files.into_iter().map(|(_, path)| path).collect();
        let inner =
            scan_store_files(state, &paths, Some(store_projection.clone()), &forwarded, None)
                .await?;

        // The message-row condition must hold exactly, so apply it as
        // a filter over the scan. The matched-coordinate `line IN`
        // restriction rides in the same conjunction; DataFusion's
        // physical filter-pushdown moves both into the Parquet source
        // as its pruning predicate (row-group pruning on `line` and
        // `type` statistics).
        let mut exact = message_row_filter();
        if let Some(expr) = line_filter {
            exact = exact.and(expr);
        }
        let inner_schema = inner.schema();
        let df_schema = DFSchema::try_from(inner_schema.clone())?;
        let predicate = create_physical_expr(&exact, &df_schema, &ExecutionProps::new())?;
        let filtered: Arc<dyn ExecutionPlan> = Arc::new(FilterExec::try_new(predicate, inner)?);

        // Project away the helper columns if the request didn't
        // include them.
        if store_projection.len() == requested {
            return Ok(filtered);
        }
        let exprs: Vec<(Arc<dyn PhysicalExpr>, String)> = (0..requested)
            .map(|i| {
                let name = inner_schema.field(i).name().clone();
                let expr = datafusion::physical_expr::expressions::col(&name, &inner_schema)?;
                Ok((expr, name))
            })
            .collect::<Result<_>>()?;
        Ok(Arc::new(ProjectionExec::try_new(exprs, filtered)?))
    }
}

/// Run the index search for each accelerated conjunct and intersect
/// the coordinate sets (conjuncts AND together).
async fn search_coordinates(
    store: &Arc<IndexStore>,
    queries: Vec<String>,
) -> Result<HashSet<(String, i64)>> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || -> gage_index::Result<HashSet<(String, i64)>> {
        let mut acc: Option<HashSet<(String, i64)>> = None;
        for query in &queries {
            let set: HashSet<(String, i64)> = store.search(query)?.into_iter().collect();
            acc = Some(match acc {
                Some(prev) => prev.intersection(&set).cloned().collect(),
                None => set,
            });
        }
        Ok(acc.unwrap_or_default())
    })
    .await
    .map_err(|e| DataFusionError::Execution(format!("index search task failed: {e}")))?
    .map_err(|e| DataFusionError::Execution(format!("index search failed: {e}")))
}

/// Derive one session's message rows in memory — the per-session
/// scanner context, which bypasses the store.
async fn derive_message_batch(session_id: &str, path: &Path) -> Result<RecordBatch> {
    let session_id = session_id.to_string();
    let path = path.to_path_buf();
    let derived = tokio::task::spawn_blocking(move || gage_index::derive_session(&session_id, &path))
        .await
        .map_err(|e| DataFusionError::Execution(format!("derive task failed: {e}")))?;
    let batch = match derived {
        Ok(derived) => derived.batch,
        Err(e) => {
            tracing::debug!("session not derivable: {e}");
            return Ok(RecordBatch::new_empty(message_schema()));
        }
    };
    // Message rows: text is non-null exactly for them.
    let texts = batch
        .column(COL_TEXT)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DataFusionError::Internal("store text column type".into()))?;
    let mask: BooleanArray = (0..batch.num_rows())
        .map(|i| Some(texts.is_valid(i)))
        .collect();
    let filtered = filter_record_batch(&batch, &mask)?;
    Ok(filtered.project(STORE_PROJECTION)?)
}
