//! Shared session-walk helpers for the session-row table providers
//! (`message`, `entry`, `session`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use crate::index::IndexStore;
use datafusion::arrow::array::{Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;
use datafusion::prelude::Expr;

use gage_core::style::IdHighlighter;

use crate::session::SessionInfo;
use gage_session::filter::{ColumnFilter, IdFilter, covered_by};

/// The columns the walk fills from the directory listing alone,
/// besides the id column: the project slug, the file mtime, and the
/// file size. A filter over these and the id is applied in the walk,
/// before any session is parsed.
pub(crate) const WALK_COLS: &[&str] = &["project", "mtime", "size"];

/// The schema of the walk's own columns, with the id column named as
/// the table names it. Types match the `session` table's.
fn walk_schema(id_col: &str) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(id_col, DataType::Utf8, false),
        Field::new("project", DataType::Utf8, true),
        Field::new(
            "mtime",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new("size", DataType::Int64, false),
    ]))
}

/// The walked sessions as a batch of the walk's columns.
fn walk_batch(schema: &SchemaRef, sessions: &[SessionInfo]) -> Result<RecordBatch> {
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let projects: Vec<String> = sessions
        .iter()
        .map(|s| s.project_name().into_owned())
        .collect();
    let mtimes: Vec<i64> = sessions
        .iter()
        .map(|s| {
            s.mtime
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        })
        .collect();
    let sizes: Vec<i64> = sessions.iter().map(|s| s.size as i64).collect();
    Ok(RecordBatch::try_new(
        Arc::clone(schema),
        vec![
            Arc::new(StringArray::from(ids)),
            Arc::new(StringArray::from(projects)),
            Arc::new(TimestampMillisecondArray::from(mtimes).with_timezone("UTC")),
            Arc::new(Int64Array::from(sizes)),
        ],
    )?)
}

/// Result of [`walk_sessions`].
pub(crate) struct WalkOutcome {
    /// Sessions after the walk-column filters and the pushed limit.
    pub sessions: Vec<SessionInfo>,
    /// Total entries the directory walk produced (before any filtering).
    pub walked: usize,
    /// Wall-clock time spent in the directory walk itself.
    pub walk_ms: u128,
    /// True when `limit` was applied to the walk result. Skipped when
    /// a filter references a parsed column, since that filter can
    /// reject rows after the walk.
    pub limit_pushed: bool,
    /// Length of the shortest prefix of each walked id that is unique
    /// among every session in the source. Empty unless requested.
    pub id_prefix_len: HashMap<String, usize>,
}

/// Walk the corpus and return one entry per session. Filters over the
/// id column and [`WALK_COLS`] are applied here, from the listing,
/// before any session is parsed; `limit` then caps the result when
/// every filter was applied here. Callers must still cap their output
/// if they need a hard limit. `with_id_prefix` computes the unique id
/// prefix of every session before the filters, so the prefix set is
/// the whole source.
pub(crate) fn walk_sessions(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    limit: Option<usize>,
    with_id_prefix: bool,
) -> Result<WalkOutcome> {
    let schema = walk_schema(id_col);
    let filter = ColumnFilter::new(filters, Arc::clone(&schema))?;
    let walk_cols: Vec<&str> = std::iter::once(id_col)
        .chain(WALK_COLS.iter().copied())
        .collect();
    let limit_pushed = limit.is_some() && filters.iter().all(|f| covered_by(f, &walk_cols));

    let walk_start = Instant::now();
    let mut sessions: Vec<SessionInfo> = store.session_list().build().into_iter().collect();
    let walk_ms = walk_start.elapsed().as_millis();
    let walked = sessions.len();

    let id_prefix_len = if with_id_prefix {
        unique_prefix_lens(&sessions)
    } else {
        HashMap::new()
    };

    if let Some(filter) = &filter {
        let mask = filter.mask(&walk_batch(&schema, &sessions)?)?;
        sessions = sessions
            .into_iter()
            .zip(mask)
            .filter_map(|(s, keep)| keep.then_some(s))
            .collect();
    }
    if limit_pushed && let Some(n) = limit {
        sessions.truncate(n);
    }

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
