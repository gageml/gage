//! The `entry` and `message` tables over stored sessions.
//!
//! One provider type serves both tables; [`RowKind`] selects the
//! shape. A scan resolves the session set through the
//! [`SessionScope`], then streams one batch per session, reading each
//! session's rows through its driver on first touch. Filters on
//! `session_id` prune the session set before any content is read;
//! filters on `line` mask rows per batch. `LIMIT` stops the stream
//! once enough rows have been produced, so a limited query reads only
//! the sessions it needs.

use std::any::Any;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::Expr;
use futures::StreamExt;
use gage_session::filter::{self, RowFilter};

use crate::rows::{RowCache, entry_rows, entry_schema, message_rows, message_schema};
use crate::scope::{SessionScope, StoredSessionRef};

const SESSION_ID_COL: &str = "session_id";

#[derive(Debug, Clone, Copy)]
pub enum RowKind {
    Entry,
    Message,
}

impl RowKind {
    fn schema(self) -> SchemaRef {
        match self {
            RowKind::Entry => entry_schema(),
            RowKind::Message => message_schema(),
        }
    }

    fn rows(self, derived: &RecordBatch) -> Result<RecordBatch> {
        match self {
            RowKind::Entry => entry_rows(derived),
            RowKind::Message => message_rows(derived),
        }
    }
}

pub struct StoredRowsTable {
    kind: RowKind,
    scope: Arc<SessionScope>,
    schema: SchemaRef,
}

impl fmt::Debug for StoredRowsTable {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("StoredRowsTable")
            .field("kind", &self.kind)
            .finish()
    }
}

impl StoredRowsTable {
    pub fn new(kind: RowKind, scope: Arc<SessionScope>) -> Self {
        Self {
            kind,
            scope,
            schema: kind.schema(),
        }
    }
}

#[async_trait]
impl TableProvider for StoredRowsTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    /// Filters over `session_id` and `line` alone are applied by the
    /// scan; anything else needs the derived columns.
    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| filter::pushdown_lines(f, SESSION_ID_COL))
            .collect())
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let sessions = self.scope.sessions(filters)?;
        let row_filter = RowFilter::new(filters, SESSION_ID_COL)?;
        let cache = row_cache(state)?;
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        Ok(Arc::new(StoredRowsExec {
            kind: self.kind,
            scope: Arc::clone(&self.scope),
            sessions: Arc::new(sessions),
            cache,
            properties: PlanProperties::new(
                EquivalenceProperties::new(projected_schema.clone()),
                Partitioning::UnknownPartitioning(1),
                EmissionType::Incremental,
                Boundedness::Bounded,
            ),
            projected_schema,
            projection: projection.cloned(),
            row_filter,
            limit,
        }))
    }
}

fn row_cache(state: &dyn Session) -> Result<Arc<RowCache>> {
    state
        .config()
        .get_extension::<RowCache>()
        .ok_or_else(|| DataFusionError::Internal("RowCache extension not installed".into()))
}

#[derive(Clone)]
struct StoredRowsExec {
    kind: RowKind,
    scope: Arc<SessionScope>,
    sessions: Arc<Vec<StoredSessionRef>>,
    cache: Arc<RowCache>,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    row_filter: Option<RowFilter>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for StoredRowsExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("StoredRowsExec")
            .field("kind", &self.kind)
            .field("sessions", &self.sessions.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl DisplayAs for StoredRowsExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "StoredRowsExec({:?})", self.kind)
    }
}

impl ExecutionPlan for StoredRowsExec {
    fn name(&self) -> &'static str {
        "StoredRowsExec"
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
        let sessions = (*self.sessions).clone();

        let batches = futures::stream::iter(sessions).map(move |session| {
            let derived = work.scope.rows(&session, &work.cache)?;
            let mut batch = work.kind.rows(&derived)?;
            // session_id and line are columns 0 and 1 of both shapes
            if let Some(f) = &work.row_filter {
                batch = f.filter_batch(&batch, 0, 1)?;
            }
            match &work.projection {
                Some(p) => Ok(batch.project(p)?),
                None => Ok(batch),
            }
        });

        let stream: futures::stream::BoxStream<'static, Result<RecordBatch>> = match self.limit {
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
