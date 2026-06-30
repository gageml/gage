//! Per-agent read scope: every query against a scoped DataFusion
//! context resolves to rows linked to a single `scan_id` via the
//! `scan_session` / `scan_note` / `scan_issue` edge tables.
//!
//! Two wrapper types apply the scope at the layer that suits each
//! backend:
//!
//! - [`ScopedTable`] wraps a disk-backed [`TableProvider`] (the
//!   JSONL-driven `session` / `entry` / `message` providers). The
//!   wrapper resolves the in-scope id set at `scan()` time and
//!   prepends an `id IN (…)` expression to the filter list before
//!   delegating to the inner provider. The inner provider's
//!   `IdFilter` consumes the expression; no SQL crosses the boundary.
//!
//! - [`ScopedSqliteSource`] (see [`crate::scope::sqlite`]) plugs into
//!   the federation rewrite layer for sqlite-backed providers. Its
//!   `logical_optimizer` hook injects the same scope filter into the
//!   logical plan the federation rule unparses into SQL.
//!
//! The id set is resolved per query (per `scan()` for [`ScopedTable`],
//! per logical-optimizer invocation for [`ScopedSqliteSource`]) so
//! the scope stays live as the scan grows during the agent's run.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::common::{Column, ScalarValue};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result as DfResult};
use datafusion::logical_expr::expr::InList;
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_plan::ExecutionPlan;

pub mod sqlite;

/// Which `scan_xxx` edge a [`Scope`] reads. Each variant points at one
/// table whose `scan_id` column is the predicate and whose paired
/// column carries the ids we keep.
#[derive(Clone, Copy, Debug)]
pub enum ScopeEdge {
    /// `scan_session(scan_id, session_id)` — the `session` / `entry` /
    /// `message` providers all key off `session_id`.
    Session,
    /// `scan_note(scan_id, note_id)` — `note` and `session_note`.
    Note,
    /// `scan_issue(scan_id, issue_id)` — `issue` and `issue_evidence`.
    Issue,
}

/// The set of ids visible to one agent run. Cheap to clone; the id
/// list is not stored — `load_ids` runs one indexed sqlite SELECT per
/// call so the scope reflects rows added during the run.
#[derive(Clone, Debug)]
pub struct Scope {
    scan_id: String,
    edge: ScopeEdge,
}

impl Scope {
    pub fn new(scan_id: impl Into<String>, edge: ScopeEdge) -> Self {
        Self {
            scan_id: scan_id.into(),
            edge,
        }
    }

    pub fn scan_id(&self) -> &str {
        &self.scan_id
    }

    pub fn edge(&self) -> ScopeEdge {
        self.edge
    }

    /// Resolve the in-scope id set against the canonical sqlite. One
    /// indexed lookup against the matching `scan_xxx` table.
    pub fn load_ids(&self) -> DfResult<Vec<String>> {
        let conn = gage_db::db::open_db().map_err(external)?;
        let ids = match self.edge {
            ScopeEdge::Session => gage_db::scan::session_ids_for_scan(&conn, &self.scan_id),
            ScopeEdge::Note => gage_db::scan::note_ids_for_scan(&conn, &self.scan_id),
            ScopeEdge::Issue => gage_db::scan::issue_ids_for_scan(&conn, &self.scan_id),
        }
        .map_err(external)?;
        Ok(ids)
    }
}

/// Build `<col> IN (<id1>, <id2>, …)` over Utf8 literals.
pub(crate) fn in_list_expr(col: Expr, ids: &[String]) -> Expr {
    let list = ids
        .iter()
        .map(|id| Expr::Literal(ScalarValue::Utf8(Some(id.clone())), None))
        .collect();
    Expr::InList(InList::new(Box::new(col), list, false))
}

fn external<E: std::error::Error + Send + Sync + 'static>(e: E) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

/// Wrap a disk-backed [`TableProvider`] with a scope filter. The
/// inner provider's `IdFilter`-driven pushdown handles the prepended
/// `InList` expression as if a caller had written it.
pub struct ScopedTable {
    inner: Arc<dyn TableProvider>,
    id_col: &'static str,
    scope: Scope,
}

impl ScopedTable {
    pub fn new(inner: Arc<dyn TableProvider>, id_col: &'static str, scope: Scope) -> Self {
        Self {
            inner,
            id_col,
            scope,
        }
    }
}

impl fmt::Debug for ScopedTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedTable")
            .field("id_col", &self.id_col)
            .field("scope", &self.scope)
            .finish()
    }
}

#[async_trait]
impl TableProvider for ScopedTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }

    fn table_type(&self) -> TableType {
        self.inner.table_type()
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DfResult<Vec<TableProviderFilterPushDown>> {
        self.inner.supports_filters_pushdown(filters)
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        let ids = self.scope.load_ids()?;
        let scope_filter = in_list_expr(Expr::Column(Column::new_unqualified(self.id_col)), &ids);
        let mut combined = Vec::with_capacity(filters.len() + 1);
        combined.push(scope_filter);
        combined.extend_from_slice(filters);
        self.inner.scan(state, projection, &combined, limit).await
    }
}
