//! Shared scan helpers for the session-row table providers
//! (`message`, `entry`, `session`).

use std::path::PathBuf;
use std::sync::Arc;

use datafusion::catalog::Session;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_claude::session::{SessionInfo, SessionListBuilder};
use gage_index::{IndexStore, LockMode};

use crate::cache::SessionCache;
use crate::filter::IdFilter;

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

/// Walk the corpus and return one entry per session, optionally
/// filtered by an id predicate over `id_col`.
pub(crate) fn walk_sessions(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
) -> Result<Vec<SessionInfo>> {
    let sessions: Vec<SessionInfo> = SessionListBuilder::new()
        .root(store.root())
        .build()
        .into_iter()
        .collect();
    match IdFilter::new(filters, id_col)? {
        Some(f) => f.retain(sessions, |s| s.id.as_str()),
        None => Ok(sessions),
    }
}

/// The same walk, projected to the `(id, path)` pairs the message and
/// entry providers need.
pub(crate) fn session_paths(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
) -> Result<Vec<(String, PathBuf)>> {
    Ok(walk_sessions(store, filters, id_col)?
        .into_iter()
        .map(|s| (s.id, s.src))
        .collect())
}
