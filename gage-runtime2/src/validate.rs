//! `partition`: memoization of completed work by validation key. See
//! validation-and-splitting.md.
//!
//! `scan().sessions().partition(KEY)` is a builder; awaiting it
//! returns a [`SessionPartition`]. A session is valid when the largest
//! `native_size` any prior scan recorded for it under the key is at
//! least its size in this scan's dataset; the read is one query over
//! the scoped `session` table and `scan_validation`.
//! `.carry_forward_notes()` on the builder links, from this scan, the
//! notes every recording scan wrote for each valid session under this
//! task's author. `mark_valid(s)` on the partition writes the size
//! observed at partition time under `validation/session/<key>/<id>` in
//! staging.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use datafusion::arrow::array::{Array, Int64Array};
use gage_runtime::error::Error;
use gage_runtime::validate::key_string;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::OUTPUT_SINK;
use crate::scan::{Session, SessionsQuery, current, run, string_column};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<PartitionQuery>()?;
    m.function_meta(PartitionQuery::carry_forward_notes)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: PartitionQuery| async move {
        do_partition(q).await
    })?;
    m.ty::<SessionPartition>()?;
    m.field_function(&Protocol::GET, "valid", |p: &SessionPartition| {
        p.valid.clone()
    })?;
    m.field_function(&Protocol::GET, "invalid", |p: &SessionPartition| {
        p.invalid.clone()
    })?;
    m.function_meta(mark_valid)?;
    m.function_meta(SessionPartition::debug)?;
    Ok(m)
}

/// The value of `sessions.partition(key)`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct PartitionQuery {
    #[rune(skip)]
    key: Value,
    #[rune(skip)]
    carry: bool,
}

/// Partition the scan's sessions by validity under `key`.
#[rune::function(instance)]
pub(crate) fn partition(_sessions: Ref<SessionsQuery>, key: Value) -> PartitionQuery {
    PartitionQuery { key, carry: false }
}

impl PartitionQuery {
    /// Also link, from this scan, the notes prior scans wrote for each
    /// valid session under this task's author.
    #[rune::function(instance)]
    fn carry_forward_notes(mut self) -> Self {
        self.carry = true;
        self
    }
}

/// The scan's sessions divided by validity under a key.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionPartition {
    /// Sessions an earlier scan processed under the key at their
    /// current size
    #[rune(skip)]
    pub valid: Vec<Session>,
    /// Sessions the task has to process
    #[rune(skip)]
    pub invalid: Vec<Session>,
    /// The key in storage form
    #[rune(get)]
    pub key: String,
    /// The size each invalid session had at partition time
    #[rune(skip)]
    observed: HashMap<String, i64>,
    /// `validation/session/<key>/` in staging
    #[rune(skip)]
    dir: PathBuf,
}

/// Record `session`, one of `invalid`, as processed at the size the
/// partition observed. A session not in `invalid` is an `Args` error.
#[rune::function(instance)]
async fn mark_valid(
    this: Ref<SessionPartition>,
    session: Value,
) -> Result<Result<(), Error>, VmError> {
    let id = match session_id(&session) {
        Ok(id) => id,
        Err(e) => return Ok(Err(e)),
    };
    let Some(size) = this.observed.get(&id) else {
        return Ok(Err(Error::Args(format!(
            "session {id} was not partitioned as invalid"
        ))));
    };
    let written = fs::create_dir_all(&this.dir)
        .and_then(|()| fs::write(this.dir.join(&id), format!("{size}\n")));
    match written {
        Ok(()) => Ok(Ok(())),
        Err(e) => Err(VmError::panic(format!("mark_valid {id}: {e}"))),
    }
}

impl SessionPartition {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "SessionPartition {{ key: {:?}, valid: {}, invalid: {} }}",
            self.key,
            self.valid.len(),
            self.invalid.len()
        )?;
        Ok(())
    }
}

type Partitioned = Result<Result<SessionPartition, Error>, VmError>;

async fn do_partition(q: PartitionQuery) -> Partitioned {
    let key = match validation_key(&q.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let sql = format!(
        "SELECT s.id, s.locator, s.native_size, v.size \
         FROM session s \
         LEFT JOIN (SELECT input_id, MAX(CAST(validator AS BIGINT)) AS size \
                    FROM scan_validation \
                    WHERE input_type = 'session' AND key = '{key}' \
                    GROUP BY input_id) v ON v.input_id = s.id",
        key = sql_str(&key)
    );
    let ctx = current()?;
    let batches = run(ctx.query_context().await?, &sql).await?;
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    let mut observed: HashMap<String, i64> = HashMap::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let locators = string_column(batch, 1);
        let int = |i: usize| {
            batch
                .column(i)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("size columns are integers")
        };
        let (sizes, recorded) = (int(2), int(3));
        for i in 0..batch.num_rows() {
            let session = Session::from_row(ids.value(i), locators.value(i));
            let size_now = sizes.value(i);
            if recorded.is_valid(i) && recorded.value(i) >= size_now {
                valid.push(session);
            } else {
                observed.insert(session.id.clone(), size_now);
                invalid.push(session);
            }
        }
    }
    if q.carry && !valid.is_empty() {
        let carried = carry_forward_notes(&key, &valid).await?;
        tracing::info!(
            key,
            sessions = valid.len(),
            notes = carried,
            "carried forward"
        );
    }
    Ok(Ok(SessionPartition {
        dir: ctx.paths.validation_dir.join("session").join(&key),
        valid,
        invalid,
        key,
        observed,
    }))
}

/// Link, from this scan, every note a recording scan wrote for a
/// valid session: the notes of each scan holding a `validation` record
/// for `(key, session)`, targeting that session, under this task's
/// author. Returns the number of note commits added to the carried
/// list.
async fn carry_forward_notes(key: &str, valid: &[Session]) -> Result<usize, VmError> {
    let (scanner, task) = OUTPUT_SINK
        .try_with(|sink| (sink.scanner.clone(), sink.task.clone()))
        .map_err(|_outside_task| {
            VmError::panic("partition is available only inside a running scan task")
        })?;
    let author = format!("task:{scanner}:{task}");
    let ids: Vec<String> = valid
        .iter()
        .map(|s| format!("'{}'", sql_str(&s.id)))
        .collect();
    let sql = format!(
        "SELECT DISTINCT l.note_commit \
         FROM scan_validation v \
         JOIN scan_note_link l ON l.scan_id = v.scan_id AND l.carried = false \
         JOIN note_target_link t ON t.note_id = l.note_id AND t.target_id = v.input_id \
         JOIN note n ON n.id = l.note_id \
         WHERE v.input_type = 'session' AND v.key = '{}' AND n.author = '{}' \
           AND v.input_id IN ({})",
        sql_str(key),
        sql_str(&author),
        ids.join(", ")
    );
    let ctx = current()?;
    let batches = run(ctx.query_context().await?, &sql).await?;
    let mut commits: BTreeSet<String> = BTreeSet::new();
    for batch in &batches {
        let col = string_column(batch, 0);
        for i in 0..batch.num_rows() {
            commits.insert(col.value(i).to_string());
        }
    }
    let path = &ctx.paths.carried_notes;
    let already: BTreeSet<String> = match fs::read_to_string(path) {
        Ok(text) => text.lines().map(String::from).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(e) => return Err(VmError::panic(format!("carried notes: {e}"))),
    };
    let new: Vec<&String> = commits.iter().filter(|c| !already.contains(*c)).collect();
    if !new.is_empty() {
        let append = || -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            for commit in &new {
                writeln!(file, "{commit}")?;
            }
            Ok(())
        };
        append().map_err(|e| VmError::panic(format!("carried notes: {e}")))?;
    }
    Ok(new.len())
}

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

/// The key in storage form. Its elements must not contain `/`, since
/// the key is a path component.
fn validation_key(key: &Value) -> Result<String, Error> {
    let key = key_string(key)?;
    if key.contains('/') {
        return Err(Error::Args(format!(
            "validation key elements must not contain '/': {key:?}"
        )));
    }
    Ok(key)
}

/// A `Session` value or a session id string.
fn session_id(v: &Value) -> Result<String, Error> {
    if let Ok(s) = v.borrow_ref::<Session>() {
        return Ok(s.id.clone());
    }
    v.borrow_string_ref().map(|s| s.to_string()).map_err(|e| {
        Error::Args(format!(
            "expected a Session or session id string, got {}: {e}",
            v.type_info()
        ))
    })
}
