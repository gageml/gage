//! `note_message_context(note_id, before, after)` — return the message
//! window around the line a note targets.
//!
//! Resolves the note's `(session_id, line, line_end)` from
//! `session_note`. The note must have a non-null `line`; whole-session
//! notes (line IS NULL) produce no rows. Returns the anchor span
//! `[line, COALESCE(line_end, line)]` plus `before` messages
//! immediately preceding it and `after` messages immediately following
//! it. `before`/`after` count messages (rows where `text IS NOT NULL`);
//! the anchor span is always included verbatim regardless of
//! message-ness.
//!
//! Seek is in-memory: the session's derived `RecordBatch` comes from
//! the shared `SessionCache` (the same one the `message` table reads
//! through), and the row search is a binary search on `line` plus
//! bounded walks for the before/after windows. No re-parse, no full
//! scan.

use std::any::Any;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::array::{Array, Int64Array, StringArray, UInt64Array};
use datafusion::arrow::compute::take;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::ScalarValue;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_claude::session::SessionListBuilder;
use gage_index::{COL_LINE, COL_TEXT, IndexStore};

use super::message::{PROJECTION as MESSAGE_PROJECTION, message_schema};
use super::walk::{session_cache, session_scope};

/// Argument-list display string for `\df`.
pub const NOTE_MESSAGE_CONTEXT_ARGS: &str = "note_id text, before integer, after integer";

pub fn note_message_context_schema() -> SchemaRef {
    SCHEMA.clone()
}

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(message_schema);

#[derive(Debug)]
pub struct NoteMessageContextFn {
    store: Arc<IndexStore>,
}

impl NoteMessageContextFn {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self { store }
    }
}

impl TableFunctionImpl for NoteMessageContextFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let [a0, a1, a2] = args else {
            return Err(DataFusionError::Plan(
                "note_message_context(note_id, before, after) takes exactly three arguments".into(),
            ));
        };
        let note_id = string_literal(a0).ok_or_else(|| {
            DataFusionError::Plan("note_message_context note_id must be a string literal".into())
        })?;
        let before = nonneg_int_literal(a1).ok_or_else(|| {
            DataFusionError::Plan(
                "note_message_context before must be a non-negative integer literal".into(),
            )
        })?;
        let after = nonneg_int_literal(a2).ok_or_else(|| {
            DataFusionError::Plan(
                "note_message_context after must be a non-negative integer literal".into(),
            )
        })?;
        Ok(Arc::new(NoteMessageContextTable {
            note_id,
            before,
            after,
            store: Arc::clone(&self.store),
        }))
    }
}

fn string_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => Some(s.clone()),
        _ => None,
    }
}

fn nonneg_int_literal(e: &Expr) -> Option<usize> {
    let n = match e {
        Expr::Literal(ScalarValue::Int64(Some(n)), _) => *n,
        Expr::Literal(ScalarValue::Int32(Some(n)), _) => *n as i64,
        Expr::Literal(ScalarValue::UInt64(Some(n)), _) => *n as i64,
        Expr::Literal(ScalarValue::UInt32(Some(n)), _) => *n as i64,
        _ => return None,
    };
    if n < 0 { None } else { Some(n as usize) }
}

#[derive(Debug)]
struct NoteMessageContextTable {
    note_id: String,
    before: usize,
    after: usize,
    store: Arc<IndexStore>,
}

#[async_trait]
impl TableProvider for NoteMessageContextTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        SCHEMA.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let target = resolve_target(&self.note_id)?;
        let batch = match target {
            Some(t) => match window_for(state, &self.store, &t, self.before, self.after).await? {
                Some(b) => b,
                None => empty_batch(),
            },
            None => empty_batch(),
        };
        let mem = MemTable::try_new(SCHEMA.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, &[], None).await
    }
}

struct Target {
    session_id: String,
    line: i64,
    line_end: i64,
}

/// Resolve `note_id` (full or prefix) to its `session_note` row. The
/// note_id is matched as a prefix against `note.id`; an ambiguous
/// prefix (more than one matching note) is an error. Only notes with
/// `line IS NOT NULL` are considered.
fn resolve_target(note_id: &str) -> Result<Option<Target>> {
    let conn = gage_db::db::open_db_at(&gage_db::db::db_path())
        .map_err(|e| DataFusionError::Execution(format!("open gage db: {e}")))?;
    let pattern = format!("{}%", escape_like(note_id));
    let mut stmt = conn
        .prepare(
            "SELECT note_id, session_id, line, line_end FROM session_note \
             WHERE note_id LIKE ?1 ESCAPE '\\' AND line IS NOT NULL \
             ORDER BY note_id LIMIT 2",
        )
        .map_err(|e| DataFusionError::Execution(format!("prepare session_note: {e}")))?;
    let mut rows = stmt
        .query([&pattern])
        .map_err(|e| DataFusionError::Execution(format!("query session_note: {e}")))?;
    let Some(first) = rows
        .next()
        .map_err(|e| DataFusionError::Execution(format!("read session_note: {e}")))?
    else {
        return Ok(None);
    };
    let first_note_id: String = first
        .get(0)
        .map_err(|e| DataFusionError::Execution(format!("session_note.note_id: {e}")))?;
    let session_id: String = first
        .get(1)
        .map_err(|e| DataFusionError::Execution(format!("session_note.session_id: {e}")))?;
    let line: i64 = first
        .get(2)
        .map_err(|e| DataFusionError::Execution(format!("session_note.line: {e}")))?;
    let line_end: Option<i64> = first
        .get(3)
        .map_err(|e| DataFusionError::Execution(format!("session_note.line_end: {e}")))?;
    if let Some(second) = rows
        .next()
        .map_err(|e| DataFusionError::Execution(format!("read session_note: {e}")))?
    {
        let second_note_id: String = second
            .get(0)
            .map_err(|e| DataFusionError::Execution(format!("session_note.note_id: {e}")))?;
        if second_note_id != first_note_id {
            return Err(DataFusionError::Plan(format!(
                "note id prefix {note_id:?} is ambiguous: matches at least \
                 {first_note_id:?} and {second_note_id:?}"
            )));
        }
    }
    Ok(Some(Target {
        session_id,
        line,
        line_end: line_end.unwrap_or(line),
    }))
}

/// Escape `%`, `_`, and `\` so the user-supplied prefix is matched
/// literally by sqlite's `LIKE ... ESCAPE '\'`.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn window_for(
    state: &dyn Session,
    store: &IndexStore,
    target: &Target,
    before: usize,
    after: usize,
) -> Result<Option<RecordBatch>> {
    if let Some(scope) = session_scope(state)
        && !scope.0.contains(&target.session_id)
    {
        return Ok(None);
    }
    let Some(path) = SessionListBuilder::new()
        .root(store.root())
        .build()
        .into_iter()
        .find(|s| s.id == target.session_id)
        .map(|s| s.src)
    else {
        return Ok(None);
    };
    let cache = session_cache(state)?;
    let derived = cache.get(&target.session_id, &path).await?;
    let batch = &derived.batch;

    let lines = batch
        .column(COL_LINE)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| DataFusionError::Internal("derived line column type".into()))?;
    let texts = batch
        .column(COL_TEXT)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| DataFusionError::Internal("derived text column type".into()))?;

    let lo = lower_bound(lines, target.line);
    let hi_exclusive = upper_bound(lines, target.line_end);
    if lo >= batch.num_rows() || hi_exclusive == 0 || lo >= hi_exclusive {
        // Anchor lines don't appear in this session's batch. Return an
        // empty result rather than fabricating context around nothing.
        return Ok(Some(empty_batch()));
    }

    let mut indices: Vec<u64> = Vec::with_capacity(before + after + (hi_exclusive - lo));

    // before window: walk backward from lo, counting only message rows.
    let mut taken = 0usize;
    let mut i = lo;
    let mut backward: Vec<u64> = Vec::with_capacity(before);
    while taken < before && i > 0 {
        i -= 1;
        if texts.is_valid(i) {
            backward.push(i as u64);
            taken += 1;
        }
    }
    backward.reverse();
    indices.extend(backward);

    // anchor span: every row whose line falls in [target.line, target.line_end].
    for i in lo..hi_exclusive {
        indices.push(i as u64);
    }

    // after window: walk forward from hi_exclusive.
    let mut taken = 0usize;
    let mut i = hi_exclusive;
    while taken < after && i < batch.num_rows() {
        if texts.is_valid(i) {
            indices.push(i as u64);
            taken += 1;
        }
        i += 1;
    }

    let idx = UInt64Array::from(indices);
    let cols = batch
        .columns()
        .iter()
        .map(|c| take(c.as_ref(), &idx, None))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let derived_batch = RecordBatch::try_new(batch.schema(), cols)?;
    Ok(Some(derived_batch.project(MESSAGE_PROJECTION)?))
}

/// Largest `i` such that `lines[i-1] < target` — i.e. the first row
/// whose line is `>= target`. The line column is monotonically
/// ascending so binary search is sound.
fn lower_bound(lines: &Int64Array, target: i64) -> usize {
    let (mut lo, mut hi) = (0usize, lines.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if lines.value(mid) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// First row whose line is `> target`.
fn upper_bound(lines: &Int64Array, target: i64) -> usize {
    let (mut lo, mut hi) = (0usize, lines.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if lines.value(mid) <= target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn empty_batch() -> RecordBatch {
    RecordBatch::new_empty(SCHEMA.clone())
}
