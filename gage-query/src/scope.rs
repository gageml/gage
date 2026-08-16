//! Per-agent read scope: every query against a scoped DataFusion
//! context resolves to rows linked to a single `scan_id` via the
//! `scan_session` / `scan_note` / `scan_issue` edge tables.
//!
//! [`ScopedTable`] wraps a [`TableProvider`] and prepends an
//! `id IN (…)` expression to the filter list on every `scan()`
//! before delegating to the inner provider. For disk-backed
//! providers (`session` / `entry` / `message`) the inner provider's
//! `IdFilter` consumes the expression against its cached listing —
//! no SQL. For sqlite-backed providers the inner provider unparses
//! the expression into `WHERE id IN (?, ?, …)` bound parameters,
//! sized under `SQLITE_LIMIT_VARIABLE_NUMBER` (32k+ on modern
//! builds).
//!
//! `scan_session` is fixed for the run and its id set is resolved
//! once at [`Scope`] construction. `scan_note` and `scan_issue` grow
//! as the agent writes; their id sets are resolved per query.

use std::any::Any;
use std::collections::HashMap;
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

/// One session an agent context is narrowed to, with an optional
/// inclusive line range. `lines: None` means the whole session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionScope {
    pub session_id: String,
    pub lines: Option<(i64, i64)>,
}

/// The set of ids visible to one agent run. Cheap to clone. The
/// `scan_session` set is loaded once at construction; `scan_note` and
/// `scan_issue` sets are loaded per query.
#[derive(Clone, Debug)]
pub struct Scope {
    scan_id: String,
    edge: ScopeEdge,
    session_ids: Option<Arc<[String]>>,
    /// Per-session inclusive line bounds for [`ScopeEdge::Session`]
    /// scopes narrowed to explicit sessions. Sessions absent from the
    /// map are unconstrained.
    ranges: Arc<HashMap<String, (i64, i64)>>,
}

impl Scope {
    /// Build a scope for `scan_id` on `edge`. For [`ScopeEdge::Session`]
    /// this loads the fixed session id set now.
    pub fn resolve(scan_id: impl Into<String>, edge: ScopeEdge) -> DfResult<Self> {
        Self::resolve_sessions(scan_id, edge, None)
    }

    /// Build a scope for `scan_id` on `edge`, narrowed to `sessions`
    /// when given. Only [`ScopeEdge::Session`] scopes consume the
    /// narrowing: the session id set becomes the given sessions and
    /// their line bounds populate the range map.
    pub fn resolve_sessions(
        scan_id: impl Into<String>,
        edge: ScopeEdge,
        sessions: Option<&[SessionScope]>,
    ) -> DfResult<Self> {
        let scan_id = scan_id.into();
        let mut ranges = HashMap::new();
        let session_ids = match edge {
            ScopeEdge::Session => match sessions {
                Some(list) => {
                    let ids: Vec<String> = list.iter().map(|s| s.session_id.clone()).collect();
                    for s in list {
                        if let Some(bounds) = s.lines {
                            ranges.insert(s.session_id.clone(), bounds);
                        }
                    }
                    Some(Arc::from(ids))
                }
                None => {
                    let conn = gage_db::db::open_db().map_err(external)?;
                    let ids =
                        gage_db::scan::session_ids_for_scan(&conn, &scan_id).map_err(external)?;
                    Some(Arc::from(ids))
                }
            },
            ScopeEdge::Note | ScopeEdge::Issue => None,
        };
        Ok(Self {
            scan_id,
            edge,
            session_ids,
            ranges: Arc::new(ranges),
        })
    }

    /// Whether `session_id` is in scope. Valid only for
    /// [`ScopeEdge::Session`].
    pub fn contains_session(&self, session_id: &str) -> bool {
        self.session_ids().iter().any(|id| id == session_id)
    }

    /// The inclusive line bounds `session_id` is constrained to, or
    /// `None` when the session is unconstrained.
    pub fn session_lines(&self, session_id: &str) -> Option<(i64, i64)> {
        self.ranges.get(session_id).copied()
    }

    pub fn edge(&self) -> ScopeEdge {
        self.edge
    }

    /// The in-scope session id set. Valid only for [`ScopeEdge::Session`].
    pub fn session_ids(&self) -> &[String] {
        self.session_ids
            .as_deref()
            .expect("session_ids requires ScopeEdge::Session")
    }

    /// Load the in-scope note or issue id set from the matching
    /// `scan_xxx` table. Not valid for [`ScopeEdge::Session`] — use
    /// [`Scope::session_ids`].
    pub fn load_ids(&self) -> DfResult<Vec<String>> {
        let conn = gage_db::db::open_db().map_err(external)?;
        match self.edge {
            ScopeEdge::Session => unreachable!("use session_ids() for ScopeEdge::Session"),
            ScopeEdge::Note => {
                gage_db::scan::note_ids_for_scan(&conn, &self.scan_id).map_err(external)
            }
            ScopeEdge::Issue => {
                gage_db::scan::issue_ids_for_scan(&conn, &self.scan_id).map_err(external)
            }
        }
    }
}

/// Build `<col> IN (<id1>, <id2>, …)` over Utf8 literals.
fn in_list_expr(col: Expr, ids: &[String]) -> Expr {
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
/// `InList` expression as if a caller had written it. When the scope
/// carries line ranges and the table has a line column (`line_col`),
/// a per-session line-bound expression is prepended as well; the
/// inner provider applies it row-exactly (`RowFilter`).
pub struct ScopedTable {
    inner: Arc<dyn TableProvider>,
    id_col: &'static str,
    line_col: Option<&'static str>,
    scope: Scope,
}

impl ScopedTable {
    pub fn new(inner: Arc<dyn TableProvider>, id_col: &'static str, scope: Scope) -> Self {
        Self {
            inner,
            id_col,
            line_col: None,
            scope,
        }
    }

    /// Declare the table's line column so the scope's line ranges
    /// apply. Only meaningful for [`ScopeEdge::Session`] scopes over
    /// tables with per-line rows (`entry`, `message`).
    pub fn with_line_col(mut self, line_col: &'static str) -> Self {
        self.line_col = Some(line_col);
        self
    }

    /// The per-session range constraint: rows are visible when their
    /// session is unconstrained, or their line falls inside the
    /// session's bounds. `None` when no session is constrained.
    fn range_expr(&self, ids: &[String]) -> Option<Expr> {
        let line_col = self.line_col?;
        if self.scope.ranges.is_empty() {
            return None;
        }
        let id_col = || Expr::Column(Column::new_unqualified(self.id_col));
        let unranged: Vec<String> = ids
            .iter()
            .filter(|id| !self.scope.ranges.contains_key(*id))
            .cloned()
            .collect();
        let mut arms: Vec<Expr> = Vec::new();
        if !unranged.is_empty() {
            arms.push(in_list_expr(id_col(), &unranged));
        }
        // Sorted for a deterministic expression shape
        let mut ranged: Vec<(&String, &(i64, i64))> = self.scope.ranges.iter().collect();
        ranged.sort_by_key(|(id, _)| id.as_str());
        for (id, (start, end)) in ranged {
            let sid_eq = id_col().eq(Expr::Literal(ScalarValue::Utf8(Some(id.clone())), None));
            let line = || Expr::Column(Column::new_unqualified(line_col));
            let lower = line().gt_eq(Expr::Literal(ScalarValue::Int64(Some(*start)), None));
            let upper = line().lt_eq(Expr::Literal(ScalarValue::Int64(Some(*end)), None));
            arms.push(sid_eq.and(lower).and(upper));
        }
        arms.into_iter().reduce(Expr::or)
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
        let owned;
        let ids: &[String] = match self.scope.edge() {
            ScopeEdge::Session => self.scope.session_ids(),
            ScopeEdge::Note | ScopeEdge::Issue => {
                owned = self.scope.load_ids()?;
                &owned
            }
        };
        let scope_filter = in_list_expr(Expr::Column(Column::new_unqualified(self.id_col)), ids);
        let mut combined = Vec::with_capacity(filters.len() + 2);
        combined.push(scope_filter);
        if let Some(ranges) = self.range_expr(ids) {
            combined.push(ranges);
        }
        combined.extend_from_slice(filters);
        self.inner.scan(state, projection, &combined, limit).await
    }
}
