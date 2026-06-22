use std::any::Any;
use std::collections::HashMap;
use std::fmt::{self, Formatter};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::array::{Array, BooleanArray, StringArray};
use datafusion::arrow::compute::filter_record_batch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use futures::StreamExt;
use gage_index::{
    COL_ATTACHMENTS, COL_IDE_TAGS, COL_LINE, COL_RAW, COL_SESSION_ID, COL_SUBTYPE, COL_TEXT,
    COL_TIMESTAMP, COL_TYPE, COL_UUID, IndexStore,
};

use super::SessionSource;
use super::walk::{lookup_paths, session_cache, session_paths, session_scope};
use crate::cache::SessionCache;
use crate::filter;

/// The derived columns serving the `message` table, in table-column
/// order. `text` is non-null exactly for message rows (`type IN
/// ('user','assistant') AND message.content well-formed`).
pub(crate) const PROJECTION: &[usize] = &[
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

static MESSAGE_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(
        gage_index::derived_schema()
            .project(PROJECTION)
            .expect("derived schema serves message projection"),
    )
});

pub(crate) fn message_schema() -> SchemaRef {
    MESSAGE_SCHEMA.clone()
}

#[derive(Debug, Clone)]
pub struct MessageTable {
    source: SessionSource,
    schema: SchemaRef,
}

impl MessageTable {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self {
            source: SessionSource::Corpus(store),
            schema: message_schema(),
        }
    }

    /// Build a `MessageTable` over an explicit `session_id` -> path
    /// map. Used by gage-scan: the cohort is known up front and the
    /// table resolves ids through the map. No `IndexStore`, no
    /// reconcile.
    pub fn with_lookup(sessions: Arc<HashMap<String, PathBuf>>) -> Self {
        Self {
            source: SessionSource::Lookup(sessions),
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
        Ok(filters
            .iter()
            .map(|f| filter::pushdown(f, "session_id"))
            .collect())
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let paths = match &self.source {
            SessionSource::Corpus(store) => {
                let scope = session_scope(state);
                session_paths(store, filters, "session_id", scope.as_deref())?
            }
            SessionSource::Lookup(sessions) => lookup_paths(sessions, filters)?,
        };
        let cache = session_cache(state)?;
        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };
        Ok(Arc::new(MessageExec::new(
            paths,
            cache,
            projected_schema,
            projection.cloned(),
            limit,
        )))
    }
}

/// Streaming plan: pulls one session at a time, parses it on a
/// blocking thread, emits a single batch of its message rows, and
/// drops the parsed `DerivedSession` before pulling the next.
#[derive(Clone)]
struct MessageExec {
    paths: Arc<Vec<(String, PathBuf)>>,
    cache: Arc<SessionCache>,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    limit: Option<usize>,
    properties: PlanProperties,
}

impl fmt::Debug for MessageExec {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("MessageExec")
            .field("sessions", &self.paths.len())
            .field("limit", &self.limit)
            .finish()
    }
}

impl MessageExec {
    fn new(
        paths: Vec<(String, PathBuf)>,
        cache: Arc<SessionCache>,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        limit: Option<usize>,
    ) -> Self {
        let properties = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            paths: Arc::new(paths),
            cache,
            projected_schema,
            projection,
            limit,
            properties,
        }
    }
}

impl DisplayAs for MessageExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "MessageExec")
    }
}

impl ExecutionPlan for MessageExec {
    fn name(&self) -> &'static str {
        "MessageExec"
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
        let paths = (*self.paths).clone();
        let cache = self.cache.clone();
        let projection = self.projection.clone();
        let schema = self.projected_schema.clone();

        let stream = futures::stream::iter(paths)
            .then(move |(id, path)| {
                let cache = cache.clone();
                async move {
                    let result = cache.get(&id, &path).await;
                    (id, result)
                }
            })
            .filter_map(|(id, result)| async move {
                match result {
                    Ok(derived) => Some(Ok(derived)),
                    Err(e) => {
                        tracing::warn!(session_id = %id, "skipping session: {e}");
                        None
                    }
                }
            })
            .map(move |res| {
                res.and_then(|derived| {
                    let batch = message_rows(&derived.batch)?;
                    match &projection {
                        Some(p) => batch.project(p).map_err(DataFusionError::from),
                        None => Ok(batch),
                    }
                })
            });

        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }
}

/// Filter a derived batch to message rows (text non-null) and project
/// to the message schema.
fn message_rows(batch: &RecordBatch) -> Result<RecordBatch> {
    let texts = batch
        .column(COL_TEXT)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DataFusionError::Internal("derived text column type".into()))?;
    let mask: BooleanArray = (0..batch.num_rows())
        .map(|i| Some(texts.is_valid(i)))
        .collect();
    let filtered = filter_record_batch(batch, &mask)?;
    Ok(filtered.project(PROJECTION)?)
}
