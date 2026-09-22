//! Shared session-walk helpers for the session-row table providers
//! (`message`, `entry`, `session`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::index::{IndexStore, LockMode};
use datafusion::catalog::Session;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;

use gage_core::style::IdHighlighter;

use crate::session::{SessionInfo, SessionListBuilder};
use crate::tables::cache::SessionCache;
use gage_session::filter::IdFilter;

/// Pull the per-context `SessionCache` from session config extensions.
pub fn session_cache(state: &dyn Session) -> Result<Arc<SessionCache>> {
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
pub async fn reconcile_for_query(store: &Arc<IndexStore>) -> Result<()> {
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
    /// Sessions after `id_filter`.
    pub sessions: Vec<SessionInfo>,
    /// Total entries the directory walk produced (before any filtering).
    pub walked: usize,
    /// Wall-clock time spent in the directory walk itself.
    pub walk_ms: u128,
    /// True when `limit` was applied to the walk result. Skipped
    /// when an `id_filter` is in effect, since it can reject rows
    /// post-walk.
    pub limit_pushed: bool,
    /// Length of the shortest prefix of each walked id that is unique
    /// among every session in the source. Empty unless requested.
    pub id_prefix_len: HashMap<String, usize>,
}

/// Walk the corpus and return one entry per session, filtered by an id
/// predicate over `id_col`. `limit` caps the result only when no
/// filter would reject rows; callers must still cap their output if
/// they need a hard limit. `with_id_prefix` computes the unique id
/// prefix of every session before the cap, so the prefix set is the
/// whole source.
pub(crate) fn walk_sessions(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    limit: Option<usize>,
    with_id_prefix: bool,
) -> Result<WalkOutcome> {
    let id_filter = IdFilter::new(filters, id_col)?;
    let limit_pushed = id_filter.is_none() && limit.is_some();

    let walk_start = Instant::now();
    let mut sessions: Vec<SessionInfo> = SessionListBuilder::new()
        .root(store.root())
        .build()
        .into_iter()
        .collect();
    let walk_ms = walk_start.elapsed().as_millis();
    let walked = sessions.len();

    let id_prefix_len = if with_id_prefix {
        unique_prefix_lens(&sessions)
    } else {
        HashMap::new()
    };

    if limit_pushed && let Some(n) = limit {
        sessions.truncate(n);
    }

    let sessions = match &id_filter {
        Some(f) => f.retain(sessions, |s| s.id.as_str())?,
        None => sessions,
    };

    Ok(WalkOutcome {
        sessions,
        walked,
        walk_ms,
        limit_pushed,
        id_prefix_len,
    })
}

/// Unique prefix length of every session id among its peers
fn unique_prefix_lens(sessions: &[SessionInfo]) -> HashMap<String, usize> {
    let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
    let highlighter = IdHighlighter::new(ids);
    sessions
        .iter()
        .map(|s| (s.id.clone(), highlighter.unique_prefix_len(&s.id)))
        .collect()
}

/// The same walk, projected to the `(id, path)` pairs the message and
/// entry providers need.
pub(crate) fn session_paths(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
) -> Result<Vec<(String, PathBuf)>> {
    Ok(walk_sessions(store, filters, id_col, None, false)?
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
