//! Provider wrapper that hides system-tier columns.
//!
//! A table schema's columns are tagged with
//! [`gage_session::is_system`] when they belong to the system tier
//! ([`gage_session::system_col`] for the convention). [`SkipSystemCols`]
//! wraps a provider and removes those fields, so in a user-facing
//! context the columns are absent, not merely hidden from `SELECT *`.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::Result;
use datafusion::logical_expr::TableProviderFilterPushDown;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_session::is_system;

/// A provider exposing every column of `inner` except those tagged as
/// system. Filters and limits pass through unchanged: a filter can
/// only name a visible column, and every visible column exists in the
/// inner schema under the same name.
#[derive(Debug)]
pub struct SkipSystemCols {
    inner: Arc<dyn TableProvider>,
    schema: SchemaRef,
    // Index into the inner schema of each visible column
    inner_indices: Vec<usize>,
}

impl SkipSystemCols {
    pub fn new(inner: Arc<dyn TableProvider>) -> Self {
        let full = inner.schema();
        let inner_indices: Vec<usize> = full
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| !is_system(f))
            .map(|(i, _)| i)
            .collect();
        let schema = Arc::new(
            full.project(&inner_indices)
                .expect("indices enumerate the inner schema"),
        );
        Self {
            inner,
            schema,
            inner_indices,
        }
    }
}

#[async_trait]
impl TableProvider for SkipSystemCols {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        self.inner.table_type()
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        self.inner.supports_filters_pushdown(filters)
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let inner_projection: Vec<usize> = match projection {
            Some(indices) => indices.iter().map(|&i| self.inner_indices[i]).collect(),
            None => self.inner_indices.clone(),
        };
        self.inner
            .scan(state, Some(&inner_projection), filters, limit)
            .await
    }
}
