//! Shared scan helpers for the session-row table providers
//! (`message`, `entry`, `session`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

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
pub(crate) struct SessionScope(pub(crate) HashSet<String>);

/// Pull the per-context `SessionCache` from session config extensions.
pub(crate) fn session_cache(state: &dyn Session) -> Result<Arc<SessionCache>> {
    state
        .config()
        .get_extension::<SessionCache>()
        .ok_or_else(|| {
            DataFusionError::Internal("SessionCache extension not installed on session".into())
        })
}

/// The session-id allowlist for this context, if one was installed.
pub(crate) fn session_scope(state: &dyn Session) -> Option<Arc<SessionScope>> {
    state.config().get_extension::<SessionScope>()
}

/// Drop sessions whose id falls outside `scope`. A no-op when `scope`
/// is `None` (the whole-corpus case).
fn retain_scope(sessions: Vec<SessionInfo>, scope: Option<&SessionScope>) -> Vec<SessionInfo> {
    match scope {
        Some(s) => sessions
            .into_iter()
            .filter(|x| s.0.contains(&x.id))
            .collect(),
        None => sessions,
    }
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

/// Walk the corpus and return one entry per session, filtered by an id
/// predicate over `id_col` and, when present, the context's session
/// `scope`.
pub(crate) fn walk_sessions(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    scope: Option<&SessionScope>,
) -> Result<Vec<SessionInfo>> {
    let sessions: Vec<SessionInfo> = SessionListBuilder::new()
        .root(store.root())
        .build()
        .into_iter()
        .collect();
    let sessions = match IdFilter::new(filters, id_col)? {
        Some(f) => f.retain(sessions, |s| s.id.as_str())?,
        None => sessions,
    };
    Ok(retain_scope(sessions, scope))
}

/// The same walk, projected to the `(id, path)` pairs the message and
/// entry providers need.
pub(crate) fn session_paths(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    scope: Option<&SessionScope>,
) -> Result<Vec<(String, PathBuf)>> {
    Ok(walk_sessions(store, filters, id_col, scope)?
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
