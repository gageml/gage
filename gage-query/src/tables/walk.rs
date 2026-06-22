//! Shared scan helpers for the session-row table providers
//! (`message`, `entry`, `session`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use datafusion::catalog::Session;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_claude::session::{SessionInfo, SessionListBuilder};
use gage_index::{IndexStore, LockMode};

use crate::cache::SessionCache;
use crate::filter::IdFilter;

/// Session-id allowlist installed on a context's config when the corpus
/// must be scoped to a scan's session set (the judge-agent path, which
/// runs under `GAGE_SCAN_ID`). Absent for normal whole-corpus contexts.
/// Constructed by `context.rs`; everything else reaches it via
/// [`scope_filter`] or [`walk_sessions`].
pub(crate) struct SessionScope(pub(crate) HashSet<String>);

/// Opaque view of the context's session scope. Use this — not
/// `SessionScope` — anywhere a single id needs to be tested for
/// membership. When no scope is installed every id is in-scope.
pub(crate) struct ScopeFilter(Option<Arc<SessionScope>>);

impl ScopeFilter {
    pub(crate) fn contains(&self, session_id: &str) -> bool {
        match &self.0 {
            Some(s) => s.0.contains(session_id),
            None => true,
        }
    }

    fn is_unscoped(&self) -> bool {
        self.0.is_none()
    }
}

/// The scope filter for this context. Cheap; always returns a value.
pub(crate) fn scope_filter(state: &dyn Session) -> ScopeFilter {
    ScopeFilter(state.config().get_extension::<SessionScope>())
}

/// Pull the per-context `SessionCache` from session config extensions.
pub(crate) fn session_cache(state: &dyn Session) -> Result<Arc<SessionCache>> {
    state
        .config()
        .get_extension::<SessionCache>()
        .ok_or_else(|| {
            DataFusionError::Internal("SessionCache extension not installed on session".into())
        })
}

/// Try-reconcile the corpus before a query. Lock contention is
/// expected (another query is reconciling the same corpus); we serve
/// the current committed snapshot instead.
pub(crate) async fn reconcile_for_query(store: &Arc<IndexStore>) -> Result<()> {
    let store = Arc::clone(store);
    let outcome = tokio::task::spawn_blocking(move || store.reconcile(LockMode::Try))
        .await
        .map_err(|e| DataFusionError::Execution(format!("reconcile task failed: {e}")))?
        .map_err(|e| DataFusionError::Execution(format!("reconcile failed: {e}")))?;
    if outcome.skipped {
        tracing::debug!("reconcile lock contended; scanning current committed snapshot");
    }
    Ok(())
}

/// Result of [`walk_sessions`].
pub(crate) struct WalkOutcome {
    /// Sessions after `id_filter` and scope filtering.
    pub sessions: Vec<SessionInfo>,
    /// Total entries the directory walk produced (before any filtering).
    pub walked: usize,
    /// Wall-clock time spent in the directory walk itself.
    pub walk_ms: u128,
    /// True when `limit` was pushed into the directory walker. Skipped
    /// whenever an `id_filter` or session scope is in effect, since
    /// either can reject rows post-walk.
    pub limit_pushed: bool,
}

/// A scope-aware session walker captured at scan-planning time. Use
/// this when the walk needs to happen later than `scan()` (i.e. inside
/// an `ExecutionPlan::execute`), so the plan can hold whatever context
/// state the walk depends on without re-borrowing `&dyn Session`.
#[derive(Clone)]
pub(crate) struct SessionWalker {
    scope: Arc<ScopeFilter>,
}

impl SessionWalker {
    pub(crate) fn walk(
        &self,
        store: &IndexStore,
        filters: &[Expr],
        id_col: &str,
        limit: Option<usize>,
    ) -> Result<WalkOutcome> {
        walk_inner(&self.scope, store, filters, id_col, limit)
    }
}

/// Capture a [`SessionWalker`] from the current context.
pub(crate) fn session_walker(state: &dyn Session) -> SessionWalker {
    SessionWalker {
        scope: Arc::new(scope_filter(state)),
    }
}

/// Walk the corpus and return one entry per session, filtered by an id
/// predicate over `id_col` and by the context's session scope (if one
/// is installed). `limit` is a hint pushed into the directory walker
/// only when no filter would reject rows; callers must still cap their
/// output if they need a hard limit.
pub(crate) fn walk_sessions(
    state: &dyn Session,
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    limit: Option<usize>,
) -> Result<WalkOutcome> {
    walk_inner(&scope_filter(state), store, filters, id_col, limit)
}

fn walk_inner(
    scope: &ScopeFilter,
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    limit: Option<usize>,
) -> Result<WalkOutcome> {
    let id_filter = IdFilter::new(filters, id_col)?;

    let mut builder = SessionListBuilder::new().root(store.root());
    let limit_pushed = id_filter.is_none() && scope.is_unscoped() && limit.is_some();
    if limit_pushed && let Some(n) = limit {
        builder = builder.limit(n);
    }

    let walk_start = Instant::now();
    let sessions: Vec<SessionInfo> = builder.build().into_iter().collect();
    let walk_ms = walk_start.elapsed().as_millis();
    let walked = sessions.len();

    let sessions = match &id_filter {
        Some(f) => f.retain(sessions, |s| s.id.as_str())?,
        None => sessions,
    };
    let sessions: Vec<SessionInfo> = sessions
        .into_iter()
        .filter(|s| scope.contains(&s.id))
        .collect();

    Ok(WalkOutcome {
        sessions,
        walked,
        walk_ms,
        limit_pushed,
    })
}

/// The same walk, projected to the `(id, path)` pairs the message and
/// entry providers need.
pub(crate) fn session_paths(
    state: &dyn Session,
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
) -> Result<Vec<(String, PathBuf)>> {
    Ok(walk_sessions(state, store, filters, id_col, None)?
        .sessions
        .into_iter()
        .map(|s| (s.id, s.src))
        .collect())
}

/// `(id, path)` pairs resolved through an explicit id-to-path map,
/// filtered by predicates over `session_id`.
pub(crate) fn lookup_paths(
    sessions: &Arc<HashMap<String, PathBuf>>,
    filters: &[Expr],
) -> Result<Vec<(String, PathBuf)>> {
    let mut pairs: Vec<(String, PathBuf)> = sessions
        .iter()
        .map(|(id, path)| (id.clone(), path.clone()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    match IdFilter::new(filters, "session_id")? {
        Some(f) => f.retain(pairs, |(id, _)| id.as_str()),
        None => Ok(pairs),
    }
}
