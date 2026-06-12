//! `message_text(query)` — table-valued full-text search over message
//! text.
//!
//! Returns `(session_id, line, type, subtype, score, snippet)`. One scan = one
//! tantivy search; scores are BM25, matched terms in snippets are
//! wrapped in guillemets (`«term»`). `LIMIT n` is pushed through to
//! `TopDocs::with_limit(n)`; omitted, the default cap is
//! [`DEFAULT_LIMIT`] and truncation is logged.

use std::any::Any;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::array::{Float32Builder, Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::ScalarValue;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_index::{DEFAULT_SNIPPET_CHARS, IndexStore};

use super::walk::reconcile_for_query;

/// Default cap when no `LIMIT` is supplied. Explicit user limits pass
/// through verbatim with no ceiling.
const DEFAULT_LIMIT: usize = 100;

static MESSAGE_TEXT_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("type", DataType::Utf8, true),
        Field::new("subtype", DataType::Utf8, true),
        Field::new("score", DataType::Float32, false),
        Field::new("snippet", DataType::Utf8, false),
    ]))
});

/// TVF registration: `register_udtf("message_text", MessageTextFn::new(store))`.
#[derive(Debug)]
pub struct MessageTextFn {
    store: Arc<IndexStore>,
}

impl MessageTextFn {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self { store }
    }
}

impl TableFunctionImpl for MessageTextFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let query = match args.first() {
            Some(Expr::Literal(ScalarValue::Utf8(Some(q)), _))
            | Some(Expr::Literal(ScalarValue::LargeUtf8(Some(q)), _))
            | Some(Expr::Literal(ScalarValue::Utf8View(Some(q)), _)) => q.clone(),
            _ => {
                return Err(DataFusionError::Plan(
                    "message_text(query [, snippet_len]) requires a string literal query".into(),
                ));
            }
        };
        let snippet_len = match args.get(1) {
            None => DEFAULT_SNIPPET_CHARS,
            Some(Expr::Literal(ScalarValue::Int64(Some(n)), _)) if *n > 0 => *n as usize,
            Some(Expr::Literal(ScalarValue::Int32(Some(n)), _)) if *n > 0 => *n as usize,
            Some(Expr::Literal(ScalarValue::UInt64(Some(n)), _)) if *n > 0 => *n as usize,
            Some(Expr::Literal(ScalarValue::UInt32(Some(n)), _)) if *n > 0 => *n as usize,
            _ => {
                return Err(DataFusionError::Plan(
                    "message_text snippet_len must be a positive integer literal".into(),
                ));
            }
        };
        if args.len() > 2 {
            return Err(DataFusionError::Plan(
                "message_text takes at most two arguments: query [, snippet_len]".into(),
            ));
        }
        Ok(Arc::new(MessageTextTable {
            query,
            snippet_len,
            store: Arc::clone(&self.store),
        }))
    }
}

#[derive(Debug)]
struct MessageTextTable {
    query: String,
    snippet_len: usize,
    store: Arc<IndexStore>,
}

#[async_trait]
impl TableProvider for MessageTextTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        MESSAGE_TEXT_SCHEMA.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        reconcile_for_query(&self.store).await?;

        let (effective, default_capped) = match limit {
            Some(n) => (n, false),
            None => (DEFAULT_LIMIT, true),
        };

        let store = Arc::clone(&self.store);
        let query = self.query.clone();
        let snippet_len = self.snippet_len;
        let hits =
            tokio::task::spawn_blocking(move || store.search(&query, effective, snippet_len))
                .await
                .map_err(|e| DataFusionError::Execution(format!("search task failed: {e}")))?
                .map_err(|e| DataFusionError::Execution(format!("search failed: {e}")))?;

        if default_capped && hits.len() == effective {
            tracing::info!(
                "message_text returned the default limit of {DEFAULT_LIMIT} hits; \
                 add an explicit `LIMIT n` for more"
            );
        }

        let batch = build_batch(hits)?;
        let mem = MemTable::try_new(MESSAGE_TEXT_SCHEMA.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, &[], None).await
    }
}

fn build_batch(hits: Vec<gage_index::Hit>) -> Result<RecordBatch> {
    let len = hits.len();
    let mut session_ids = StringBuilder::with_capacity(len, len * 36);
    let mut lines = Int64Builder::with_capacity(len);
    let mut types = StringBuilder::with_capacity(len, len * 8);
    let mut subtypes = StringBuilder::with_capacity(len, len * 8);
    let mut scores = Float32Builder::with_capacity(len);
    let mut snippets = StringBuilder::with_capacity(len, len * 64);
    for hit in hits {
        session_ids.append_value(&hit.session_id);
        lines.append_value(hit.line);
        types.append_option(hit.type_.as_deref());
        subtypes.append_option(hit.subtype.as_deref());
        scores.append_value(hit.score);
        snippets.append_value(&hit.snippet);
    }
    Ok(RecordBatch::try_new(
        MESSAGE_TEXT_SCHEMA.clone(),
        vec![
            Arc::new(session_ids.finish()),
            Arc::new(lines.finish()),
            Arc::new(types.finish()),
            Arc::new(subtypes.finish()),
            Arc::new(scores.finish()),
            Arc::new(snippets.finish()),
        ],
    )?)
}
