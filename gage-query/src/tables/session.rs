use std::any::Any;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use datafusion::arrow::array::{
    BooleanBuilder, Int64Builder, StringBuilder, TimestampMillisecondBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::Result;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::*;
use gage_index::{IndexStore, SessionAggregates};

use super::walk::{reconcile_for_query, session_cache, walk_sessions};
use crate::filter;

const EXPENSIVE_START: usize = 5; // title is the first expensive column

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
        Ok(filters.iter().map(|f| filter::pushdown(f, "id")).collect())
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        reconcile_for_query(&self.store).await?;
        let cheap_only = projection
            .as_ref()
            .is_some_and(|p| p.iter().all(|&i| i < EXPENSIVE_START));

        let sessions = walk_sessions(&self.store, filters, "id")?;
        let len = sessions.len();
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

        let cache = if cheap_only {
            None
        } else {
            Some(session_cache(state)?)
        };
        let default_aggregates = SessionAggregates {
            is_empty: true,
            ..Default::default()
        };

        for s in &sessions {
            let aggregates = match &cache {
                None => default_aggregates.clone(),
                Some(c) => match c.get(&s.id, &s.src).await {
                    Ok(d) => d.aggregates.clone(),
                    Err(e) => {
                        tracing::warn!(session_id = %s.id, "session aggregates unavailable: {e}");
                        default_aggregates.clone()
                    }
                },
            };
            ids.append_value(&s.id);
            projects.append_value(s.project_name().as_ref());
            paths.append_value(s.src.to_string_lossy().as_ref());
            let millis = s
                .mtime
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            mtimes.append_value(millis);
            sizes.append_value(s.size as i64);
            match &aggregates.title {
                Some(t) => titles.append_value(t),
                None => titles.append_null(),
            }
            match &aggregates.model {
                Some(m) => models.append_value(m),
                None => models.append_null(),
            }
            message_counts.append_value(aggregates.message_count);
            input_tokens.append_value(aggregates.input_tokens);
            output_tokens.append_value(aggregates.output_tokens);
            cache_read.append_value(aggregates.cache_read_input_tokens);
            cache_creation.append_value(aggregates.cache_creation_input_tokens);
            is_empty.append_value(aggregates.is_empty);
        }

        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::new(ids.finish()),
                Arc::new(projects.finish()),
                Arc::new(paths.finish()),
                Arc::new(mtimes.finish().with_timezone("UTC")),
                Arc::new(sizes.finish()),
                Arc::new(titles.finish()),
                Arc::new(models.finish()),
                Arc::new(message_counts.finish()),
                Arc::new(input_tokens.finish()),
                Arc::new(output_tokens.finish()),
                Arc::new(cache_read.finish()),
                Arc::new(cache_creation.finish()),
                Arc::new(is_empty.finish()),
            ],
        )?;

        let mem = MemTable::try_new(self.schema.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, &[], limit).await
    }
}
