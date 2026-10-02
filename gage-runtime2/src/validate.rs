//! Work reuse: `carry_forward_notes`, `unseen`, and `watermark`.
//!
//! A watermark is the record `watermarks/sessions/<oid>/<key>` in a
//! scan's tree, holding the session commit a task under `key`
//! finished processing. `watermark(key, s)` writes one into staging.
//! `scan().sessions().unseen(key)` reads every prior scan's
//! watermarks through the `scan_watermark` table and, per session,
//! finds the closest one in the session's commit chain: none means
//! the whole session is unseen, the scan's own commit means nothing
//! is, and an ancestor means the lines after its `line_count`.
//! `carry_forward_notes(key)` links into this scan every note whose
//! work key is `key` and whose target commit is in a session's chain. A commit that is
//! not in the chain, such as a later commit of the same session, is
//! never consulted.
//!
//! A scan run with `invalidate` set reports every session as wholly
//! unseen and carries no notes; it still writes watermarks.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use datafusion::arrow::array::{Array, Int64Array};
use gage_runtime::error::Error;
use gage_runtime::validate::key_string;
use gage_store::{SessionStore, Store};
use rune::runtime::{Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::scan::{
    ScanContext, Session, SessionsQuery, current, run, session_id, session_order, string_column,
};

const SESSIONS_KIND: &str = "sessions";

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("carry_forward_notes", carry_forward_notes)
        .build()?;
    m.function("watermark", watermark).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<CarryForwardNotes>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: CarryForwardNotes| async move {
        do_carry_forward_notes(q).await
    })?;
    m.ty::<UnseenQuery>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: UnseenQuery| async move {
        do_unseen(q).await
    })?;
    m.ty::<WatermarkWrite>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: WatermarkWrite| async move {
        do_watermark(w).await
    })?;
    Ok(m)
}

/// The value of `carry_forward_notes(key)`. Awaiting it links the
/// notes.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct CarryForwardNotes {
    #[rune(skip)]
    key: Value,
}

fn carry_forward_notes(key: Value) -> CarryForwardNotes {
    CarryForwardNotes { key }
}

/// The value of `scan().sessions().unseen(key)`. Awaiting it
/// reads the watermarks.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct UnseenQuery {
    #[rune(skip)]
    key: Value,
    #[rune(skip)]
    newest_first: bool,
}

/// Pair each of the scan's sessions with its unseen lines under `key`,
/// in the order the sessions query would read them.
#[rune::function(instance)]
pub(crate) fn unseen(sessions: Ref<SessionsQuery>, key: Value) -> UnseenQuery {
    UnseenQuery {
        key,
        newest_first: sessions.newest_first,
    }
}

/// The value of `watermark(session, key)`. Awaiting it writes the
/// record.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct WatermarkWrite {
    #[rune(skip)]
    key: Value,
    #[rune(skip)]
    session: String,
}

/// `session` is a `Session` or an id string.
fn watermark(session: Value, key: Value) -> Result<WatermarkWrite, VmError> {
    Ok(WatermarkWrite {
        key,
        session: session_id(&session)?,
    })
}

/// Link into this scan every note whose work key is `key` and whose
/// target is one of the scan's sessions at a commit in that session's
/// chain. The
/// note commits are appended to the staged carried list, which apply
/// writes as `notes_carried.link`. Returns the number of notes newly
/// linked; a note already carried by this scan counts zero.
async fn do_carry_forward_notes(q: CarryForwardNotes) -> Result<Result<i64, Error>, VmError> {
    let key = match work_key(&q.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let ctx = current()?;
    if ctx.invalidate {
        tracing::info!(
            key,
            "carry_forward_notes skipped: scan invalidates prior work"
        );
        return Ok(Ok(0));
    }
    let members = members(&ctx, false).await?;
    if members.is_empty() {
        return Ok(Ok(0));
    }
    let sql = format!(
        "SELECT t.note_commit, t.target_id, t.target_commit \
         FROM note n JOIN note_target_link t ON t.note_id = n.id \
         WHERE n.work_key = '{}' AND t.target_type = 'session' \
           AND t.target_id IN ({})",
        sql_str(&key),
        id_list(&members)
    );
    let batches = run(ctx.query_context().await?, &sql).await?;
    let chains = chains(&ctx, &members).await?;
    let mut commits: BTreeSet<String> = BTreeSet::new();
    for batch in &batches {
        let note_commits = string_column(batch, 0);
        let target_ids = string_column(batch, 1);
        let target_commits = string_column(batch, 2);
        for i in 0..batch.num_rows() {
            let in_chain = chains
                .get(target_ids.value(i))
                .is_some_and(|chain| chain.contains(target_commits.value(i)));
            if in_chain {
                commits.insert(note_commits.value(i).to_string());
            }
        }
    }
    let added = append_carried(&ctx.paths.carried_notes, &commits)?;
    tracing::info!(key, notes = added, "carry_forward_notes");
    Ok(Ok(i64::try_from(added).unwrap()))
}

/// Append the commits not already listed to the staged carried list.
/// Returns how many were appended.
fn append_carried(path: &Path, commits: &BTreeSet<String>) -> Result<usize, VmError> {
    let already: BTreeSet<String> = match fs::read_to_string(path) {
        Ok(text) => text.lines().map(String::from).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
        Err(e) => return Err(VmError::panic(format!("carried notes: {e}"))),
    };
    let new: Vec<&String> = commits.iter().filter(|c| !already.contains(*c)).collect();
    if new.is_empty() {
        return Ok(0);
    }
    let append = || -> io::Result<()> {
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
    Ok(new.len())
}

/// For each of the scan's sessions, in query order, `(session,
/// unseen)`: `None` when
/// a watermark under `key` names the commit the scan reads,
/// `Some((start, end))` otherwise, where `start` is the line after
/// the closest watermarked ancestor's `line_count`, or 1 with no
/// such ancestor, and `end` is the session's `line_count`.
async fn do_unseen(q: UnseenQuery) -> Result<Result<Vec<Value>, Error>, VmError> {
    let key = match work_key(&q.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let ctx = current()?;
    let members = members(&ctx, q.newest_first).await?;
    if members.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let marks = if ctx.invalidate {
        tracing::info!(
            key,
            "unseen ignores watermarks: scan invalidates prior work"
        );
        HashMap::new()
    } else {
        watermarks(&ctx, &key, &members).await?
    };
    let store = ctx.store.lock().await;
    let mut out = Vec::with_capacity(members.len());
    for m in members {
        let closest = match marks.get(&m.session.id) {
            Some(set) => chain(&store, &m.session.commit)?
                .into_iter()
                .find(|sha| set.contains(sha)),
            None => None,
        };
        let unseen: Option<(i64, i64)> = match closest {
            Some(sha) if sha == m.session.commit => None,
            Some(sha) => {
                let end = m.line_count()?;
                let seen = SessionStore::from(&*store)
                    .at_commit(&sha)
                    .map_err(|e| VmError::panic(format!("read session commit {sha}: {e}")))?
                    .attrs
                    .summary
                    .line_count
                    .map(|n| n as i64);
                match seen {
                    // A watermarked ancestor with at least as many
                    // lines contradicts append-only content; the
                    // whole session is treated as unseen
                    Some(n) if n < end => Some((n + 1, end)),
                    _ => Some((1, end)),
                }
            }
            None => Some((1, m.line_count()?)),
        };
        out.push(rune::to_value((m.session, unseen)).map_err(VmError::from)?);
    }
    Ok(Ok(out))
}

/// The watermarked commits under `key` per member session id.
async fn watermarks(
    ctx: &ScanContext,
    key: &str,
    members: &[Member],
) -> Result<HashMap<String, HashSet<String>>, VmError> {
    let sql = format!(
        "SELECT oid, commit FROM scan_watermark \
         WHERE kind = '{SESSIONS_KIND}' AND key = '{}' AND oid IN ({})",
        sql_str(key),
        id_list(members)
    );
    let batches = run(ctx.query_context().await?, &sql).await?;
    let mut marks: HashMap<String, HashSet<String>> = HashMap::new();
    for batch in &batches {
        let oids = string_column(batch, 0);
        let commits = string_column(batch, 1);
        for i in 0..batch.num_rows() {
            marks
                .entry(oids.value(i).to_string())
                .or_default()
                .insert(commits.value(i).to_string());
        }
    }
    Ok(marks)
}

/// Record that the task under `key` finished processing `session` at
/// the commit this scan reads. A session that is not a member of the
/// scan is an `Args` error.
async fn do_watermark(w: WatermarkWrite) -> Result<Result<(), Error>, VmError> {
    let key = match work_key(&w.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let ctx = current()?;
    let id = w.session;
    let Some(commit) = ctx.member_commit(&id).await? else {
        return Ok(Err(Error::Args(format!(
            "session {id} is not a member of the scan"
        ))));
    };
    let dir = ctx.paths.watermarks_dir.join(SESSIONS_KIND).join(&id);
    let written = fs::create_dir_all(&dir)
        .and_then(|()| write_atomic(&dir.join(&key), format!("{commit}\n").as_bytes()));
    match written {
        Ok(()) => {
            tracing::debug!(key, session = id, commit, "watermark");
            Ok(Ok(()))
        }
        Err(e) => Err(VmError::panic(format!("watermark {id}: {e}"))),
    }
}

/// Write `bytes` to a sibling temp file and rename it over `path`,
/// the staging convention for whole files.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// One of the scan's sessions with the columns the operations read.
struct Member {
    session: Session,
    line_count: Option<i64>,
}

impl Member {
    /// The session's `line_count`, which every session written by a
    /// line-structured driver carries. Its absence is a store fault.
    fn line_count(&self) -> Result<i64, VmError> {
        self.line_count.ok_or_else(|| {
            VmError::panic(format!(
                "session {} has no line_count; its driver does not report one",
                self.session.id
            ))
        })
    }
}

/// The scan's sessions, in member order unless `newest_first` is set.
async fn members(ctx: &ScanContext, newest_first: bool) -> Result<Vec<Member>, VmError> {
    let sql = format!(
        "SELECT id, locator, line_count FROM session{}",
        session_order(newest_first)
    );
    let batches = run(ctx.query_context().await?, &sql).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let locators = string_column(batch, 1);
        let lines = batch
            .column(2)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("line_count is an integer column");
        for i in 0..batch.num_rows() {
            out.push(Member {
                session: Session::from_row(ids.value(i), locators.value(i)),
                line_count: lines.is_valid(i).then(|| lines.value(i)),
            });
        }
    }
    Ok(out)
}

/// The commit chain of each member, as a set, keyed by session id.
async fn chains(
    ctx: &ScanContext,
    members: &[Member],
) -> Result<HashMap<String, HashSet<String>>, VmError> {
    let store = ctx.store.lock().await;
    let mut out = HashMap::with_capacity(members.len());
    for m in members {
        let chain: HashSet<String> = chain(&store, &m.session.commit)?.into_iter().collect();
        out.insert(m.session.id.clone(), chain);
    }
    Ok(out)
}

/// The commit chain of a session from `commit` back to its first
/// commit: `commit` first, then each `parent` in turn.
fn chain(store: &Store, commit: &str) -> Result<Vec<String>, VmError> {
    let mut out = vec![commit.to_string()];
    let mut sha = commit.to_string();
    loop {
        let header = store
            .read_header(&sha)
            .map_err(|e| VmError::panic(format!("read session commit {sha}: {e}")))?;
        match header.parent {
            Some(parent) => {
                out.push(parent.clone());
                sha = parent;
            }
            None => return Ok(out),
        }
    }
}

fn id_list(members: &[Member]) -> String {
    members
        .iter()
        .map(|m| format!("'{}'", sql_str(&m.session.id)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

/// A work key in storage form: a string as given, or a tuple of
/// strings and integers colon-joined. The result is a path component and a
/// table value, so it must be non-empty and hold no `/`.
pub(crate) fn work_key(key: &Value) -> Result<String, Error> {
    let key = match key.borrow_string_ref() {
        Ok(s) => s.to_string(),
        Err(_not_a_string) => key_string(key)?,
    };
    if key.is_empty() || key == "." || key == ".." || key.contains('/') {
        return Err(Error::Args(format!(
            "key must be non-empty and must not contain '/': {key:?}"
        )));
    }
    Ok(key)
}
