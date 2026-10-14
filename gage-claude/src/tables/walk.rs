//! Shared session-walk helpers for the session-row table providers
//! (`message`, `entry`, `tool_use`, `session`).

use std::collections::HashMap;
use std::ffi::OsStr;
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

use crate::index::ClaudeSessionAttrs;
use crate::session::SessionInfo;
use gage_session::filter::{
    ColumnFilter, IdFilter, TimeBounds, covered_by, required_members, time_bounds,
};

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

/// The row-table columns whose pushed filters prune sessions through
/// the summary cache, before any parse: a time column bounded by the
/// session's first and last entry time, and a list column whose
/// required members the session's recorded set must hold. A session
/// without a fresh summary is kept.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SummaryPrune {
    pub time_col: Option<&'static str>,
    pub files_col: Option<&'static str>,
}

impl SummaryPrune {
    pub const NONE: Self = Self {
        time_col: None,
        files_col: None,
    };

    /// Whether the summary admits a session under `filters`.
    fn admits(&self, summary: &ClaudeSessionAttrs, bounds: &TimeBounds, paths: &[String]) -> bool {
        let in_span = match (summary.first_timestamp_ms, summary.last_timestamp_ms) {
            (Some(first), Some(last)) => bounds.admits(first, last),
            _ => true,
        };
        in_span
            && paths.iter().all(|p| {
                summary
                    .files_changed
                    .iter()
                    .any(|f| f.as_os_str() == OsStr::new(p))
            })
    }
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
    prune: SummaryPrune,
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
    let bounds = prune
        .time_col
        .map(|c| time_bounds(filters, c))
        .unwrap_or_default();
    let paths = prune
        .files_col
        .map(|c| required_members(filters, c))
        .unwrap_or_default();
    if !bounds.is_open() || !paths.is_empty() {
        sessions.retain(|s| match store.session_attrs(&s.id, s.mtime) {
            Some(summary) => prune.admits(&summary, &bounds, &paths),
            None => true,
        });
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

/// The same walk, projected to the `(id, path)` pairs the row
/// providers need.
pub(crate) fn session_paths(
    store: &IndexStore,
    filters: &[Expr],
    id_col: &str,
    prune: SummaryPrune,
) -> Result<Vec<(String, PathBuf)>> {
    Ok(walk_sessions(store, filters, id_col, None, false, prune)?
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

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::common::ScalarValue;
    use datafusion::functions_nested::expr_fn::array_has;
    use datafusion::logical_expr::{BinaryExpr, Operator};
    use std::path::Path;

    fn seed(root: &Path, id: &str) -> PathBuf {
        let dir = root.join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .unwrap();
        path
    }

    fn summary(first: i64, last: i64, files: &[&str]) -> ClaudeSessionAttrs {
        ClaudeSessionAttrs {
            first_timestamp_ms: Some(first),
            last_timestamp_ms: Some(last),
            files_changed: files.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    fn ts(ms: i64) -> Expr {
        Expr::Literal(
            ScalarValue::TimestampMillisecond(Some(ms), Some("UTC".into())),
            None,
        )
    }

    fn after(col: &str, ms: i64) -> Expr {
        Expr::BinaryExpr(BinaryExpr::new(
            Box::new(Expr::Column(col.into())),
            Operator::Gt,
            Box::new(ts(ms)),
        ))
    }

    fn wrote(path: &str) -> Expr {
        array_has(
            Expr::Column("files_changed".into()),
            Expr::Literal(ScalarValue::Utf8(Some(path.to_string())), None),
        )
    }

    /// Three sessions: one in the window that wrote the file, one in
    /// the window that did not, one before the window. A fourth has
    /// no summary and is always kept.
    #[test]
    fn summaries_prune_the_walk_by_time_and_by_file() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let store = IndexStore::new(root.path(), cache.path());
        let ids = [
            "aaaaaaaa-0000-0000-0000-000000000001",
            "aaaaaaaa-0000-0000-0000-000000000002",
            "aaaaaaaa-0000-0000-0000-000000000003",
            "aaaaaaaa-0000-0000-0000-000000000004",
        ];
        for id in ids {
            seed(root.path(), id);
        }
        let summaries = [
            (ids[0], summary(1_000, 2_000, &["/w/a.rs"])),
            (ids[1], summary(1_000, 2_000, &["/w/b.rs"])),
            (ids[2], summary(100, 500, &["/w/a.rs"])),
        ];
        for (id, s) in &summaries {
            store.put_session_attrs(id, s).unwrap();
        }
        let prune = SummaryPrune {
            time_col: Some("result_timestamp"),
            files_col: Some("files_changed"),
        };
        let walked = |filters: &[Expr]| -> Vec<String> {
            let mut ids: Vec<String> = session_paths(&store, filters, "session_id", prune)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            ids.sort();
            ids
        };

        assert_eq!(walked(&[]), ids);
        assert_eq!(
            walked(&[after("result_timestamp", 900)]),
            [ids[0], ids[1], ids[3]]
        );
        assert_eq!(walked(&[wrote("/w/a.rs")]), [ids[0], ids[2], ids[3]]);
        assert_eq!(
            walked(&[after("result_timestamp", 900), wrote("/w/a.rs")]),
            [ids[0], ids[3]]
        );
        // A bound on a column the prune does not name has no effect
        assert_eq!(walked(&[after("timestamp", 900)]), ids);
        assert_eq!(
            session_paths(
                &store,
                &[after("timestamp", 900)],
                "session_id",
                SummaryPrune::NONE
            )
            .unwrap()
            .len(),
            4
        );
    }
}
