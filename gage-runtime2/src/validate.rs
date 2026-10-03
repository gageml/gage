//! Work reuse: `carry_forward_notes`, `Mark`, `watermark`, `hwm`,
//! and `unseen`.
//!
//! A watermark is the record `watermarks/<oid>/<key>` in a scan's
//! tree, holding `<commit> <mark>`: the position the task under `key`
//! reached on the object `oid`, at the object's commit the task read.
//! The mark's axis is the object's: lines for a session, 0 or 1 for a
//! whole object such as a note. A `Mark` names an object and the
//! position its constructor derives, `line_count` for
//! `Mark::session(s)` and 1 for `Mark::note(n)`, and
//! `watermark(mark, key)` writes the record into staging.
//!
//! `scan().sessions().hwm(key)` and `scan().notes().hwm(key)` read
//! every live scan's watermarks through the `scan_watermark` table
//! and pair each object with its high-water mark under the key: the
//! largest mark whose commit is on the object's commit chain, or 0
//! with none. `unseen(key)` on each query is the derived form: the
//! sessions with `hwm < line_count` paired with the unseen line
//! range, and the notes with `hwm == 0`. A commit that is not on the
//! chain, such as a later commit of the same object, is never
//! consulted.
//!
//! `carry_forward_notes(key)` links into this scan every note whose
//! carry-forward key is `key` and whose target commit is on one of
//! the scan's sessions' chains.
//!
//! A scan run with `invalidate` set reports 0 for every object and
//! carries no notes; it still writes watermarks.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use datafusion::arrow::array::{Array, UInt64Array};
use gage_runtime::error::Error;
use gage_runtime::validate::key_string;
use gage_store::{NoteStore, Store};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::note::{Note, NotesQuery, fetch_notes};
use crate::scan::{
    ScanContext, Session, SessionsQuery, current, members, run, session_id, string_column,
};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("carry_forward_notes", carry_forward_notes)
        .build()?;
    m.function("watermark", watermark).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<Mark>()?;
    m.function_meta(Mark::session)?;
    m.function_meta(Mark::note)?;
    m.function_meta(Mark::debug)?;
    m.ty::<CarryForwardNotes>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: CarryForwardNotes| async move {
        do_carry_forward_notes(q).await
    })?;
    m.ty::<SessionsHwm>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionsHwm| async move {
        do_sessions_hwm(q).await
    })?;
    m.ty::<NotesHwm>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NotesHwm| async move {
        do_notes_hwm(q).await
    })?;
    m.ty::<WatermarkWrite>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: WatermarkWrite| async move {
        do_watermark(w).await
    })?;
    Ok(m)
}

/// An object and the position a task reached on it, for `watermark`.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Mark {
    #[rune(skip)]
    target: MarkTarget,
}

#[derive(Clone)]
enum MarkTarget {
    /// The whole session at the commit the scan reads: the mark is
    /// its `line_count`
    Session { id: String },
    /// The whole note: the mark is 1
    Note { id: String, commit: NoteCommit },
}

/// What is known of a marked note's commit when the mark is made.
#[derive(Clone)]
enum NoteCommit {
    /// A note id string: resolved against the scan's notes at the
    /// write
    Unresolved,
    /// A `Note` value: its carried commit, or `None` when this scan
    /// staged it
    Known(Option<String>),
}

impl Mark {
    /// The whole session `s`, a `Session` or a session id string, at
    /// the commit the scan reads.
    #[rune::function(path = Self::session)]
    fn session(s: Value) -> Result<Mark, VmError> {
        Ok(Mark {
            target: MarkTarget::Session {
                id: session_id(&s)?,
            },
        })
    }

    /// The whole note `n`, a `Note` or a note id string.
    #[rune::function(path = Self::note)]
    fn note(n: Value) -> Result<Mark, VmError> {
        if let Ok(note) = n.borrow_ref::<Note>() {
            return Ok(Mark {
                target: MarkTarget::Note {
                    id: note.id.clone(),
                    commit: NoteCommit::Known(note.commit.clone()),
                },
            });
        }
        if let Ok(id) = n.borrow_string_ref() {
            return Ok(Mark {
                target: MarkTarget::Note {
                    id: id.to_string(),
                    commit: NoteCommit::Unresolved,
                },
            });
        }
        Err(VmError::panic(format!(
            "expected a Note or note id string, got {}",
            n.type_info()
        )))
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        match &self.target {
            MarkTarget::Session { id } => write!(f, "Mark {{ session: {id:?} }}")?,
            MarkTarget::Note { id, .. } => write!(f, "Mark {{ note: {id:?} }}")?,
        }
        Ok(())
    }
}

/// The value of `watermark(mark, key)`. Awaiting it writes the
/// record.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct WatermarkWrite {
    #[rune(skip)]
    mark: Mark,
    #[rune(skip)]
    key: Value,
}

fn watermark(mark: Ref<Mark>, key: Value) -> WatermarkWrite {
    WatermarkWrite {
        mark: mark.clone(),
        key,
    }
}

/// Record the mark under `key`. A session that is not a member of
/// the scan, or a note the scan neither staged nor carried, is an
/// `Args` error. A note this scan staged has no commit until apply,
/// so its record is deferred through the `note_watermarks` staging
/// file; apply resolves the commit and writes the record.
async fn do_watermark(w: WatermarkWrite) -> Result<Result<(), Error>, VmError> {
    let key = match encode_key(&w.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let ctx = current()?;
    let (oid, commit, mark) = match w.mark.target {
        MarkTarget::Session { id } => {
            let Some(member) = members(&ctx, false).await?.into_iter().find(|s| s.id == id) else {
                return Ok(Err(Error::Args(format!(
                    "session {id} is not a member of the scan"
                ))));
            };
            let mark = u64::try_from(member.line_count).unwrap();
            (id, member.commit, mark)
        }
        MarkTarget::Note { id, commit } => {
            if ctx.paths.notes_dir.join(&id).is_dir() {
                let line = format!("{id} {key} 1\n");
                append(&ctx.paths.note_watermarks, &line)
                    .map_err(|e| VmError::panic(format!("note watermarks: {e}")))?;
                tracing::debug!(key, note = id, "watermark deferred to apply");
                return Ok(Ok(()));
            }
            let commit = match commit {
                NoteCommit::Known(Some(commit)) => commit,
                NoteCommit::Known(None) => {
                    return Err(VmError::panic(format!(
                        "note {id} was staged by this scan but its staging is gone"
                    )));
                }
                NoteCommit::Unresolved => match carried_commit(&ctx, &id).await? {
                    Some(commit) => commit,
                    None => {
                        return Ok(Err(Error::Args(format!(
                            "note {id} is not staged or carried by the scan"
                        ))));
                    }
                },
            };
            (id, commit, 1)
        }
    };
    let dir = ctx.paths.watermarks_dir.join(&oid);
    let written = fs::create_dir_all(&dir)
        .and_then(|()| write_atomic(&dir.join(&key), format!("{commit} {mark}\n").as_bytes()));
    match written {
        Ok(()) => {
            tracing::debug!(key, oid, commit, mark, "watermark");
            Ok(Ok(()))
        }
        Err(e) => Err(VmError::panic(format!("watermark {oid}: {e}"))),
    }
}

/// The carried commit of the note `id`, or `None` when the scan has
/// not carried it.
async fn carried_commit(ctx: &ScanContext, id: &str) -> Result<Option<String>, VmError> {
    let text = match fs::read_to_string(&ctx.paths.carried_notes) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(VmError::panic(format!("carried notes: {e}"))),
    };
    let store = ctx.store.lock().await;
    let notes = NoteStore::from(&*store);
    for sha in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let note = notes
            .at_commit(sha)
            .map_err(|e| VmError::panic(format!("carried note {sha}: {e}")))?;
        if note.id == id {
            return Ok(Some(sha.to_string()));
        }
    }
    Ok(None)
}

fn append(path: &Path, line: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

/// Write `bytes` to a sibling temp file and rename it over `path`,
/// the staging convention for whole files.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// The value of `scan().sessions().hwm(key)` and `.unseen(key)`.
/// Awaiting it reads the watermarks.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionsHwm {
    #[rune(skip)]
    key: Value,
    #[rune(skip)]
    newest_first: bool,
    #[rune(skip)]
    shape: Shape,
}

/// What a watermark read yields per object
#[derive(Clone, Copy)]
enum Shape {
    /// Every object with its high-water mark
    Hwm,
    /// The objects with work above the mark
    Unseen,
}

/// Pair each of the scan's sessions with its high-water mark under
/// `key`, in the order the sessions query would read them.
#[rune::function(instance, path = hwm)]
pub(crate) fn sessions_hwm(sessions: Ref<SessionsQuery>, key: Value) -> SessionsHwm {
    SessionsHwm {
        key,
        newest_first: sessions.newest_first,
        shape: Shape::Hwm,
    }
}

/// The scan's sessions with lines above their high-water mark under
/// `key`, each paired with `(start, end)`, the inclusive unseen line
/// range, in the order the sessions query would read them.
#[rune::function(instance, path = unseen)]
pub(crate) fn sessions_unseen(sessions: Ref<SessionsQuery>, key: Value) -> SessionsHwm {
    SessionsHwm {
        key,
        newest_first: sessions.newest_first,
        shape: Shape::Unseen,
    }
}

async fn do_sessions_hwm(q: SessionsHwm) -> Result<Result<Vec<Value>, Error>, VmError> {
    let key = match encode_key(&q.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let ctx = current()?;
    let sessions = members(&ctx, q.newest_first).await?;
    let with_commit: Vec<(&str, Option<&str>)> = sessions
        .iter()
        .map(|s| (s.id.as_str(), Some(s.commit.as_str())))
        .collect();
    let marks = hwm(&ctx, &key, &with_commit).await?;
    let mut out = Vec::with_capacity(sessions.len());
    for (s, hwm) in sessions.into_iter().zip(marks) {
        let value = match q.shape {
            Shape::Hwm => rune::to_value((s, hwm)),
            Shape::Unseen if hwm < s.line_count => {
                let range = (hwm + 1, s.line_count);
                rune::to_value((s, range))
            }
            Shape::Unseen => continue,
        };
        out.push(value.map_err(VmError::from)?);
    }
    Ok(Ok(out))
}

/// The value of `scan().notes().hwm(key)` and `.unseen(key)`.
/// Awaiting it reads the notes and their watermarks.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct NotesHwm {
    #[rune(skip)]
    query: NotesQuery,
    #[rune(skip)]
    key: Value,
    #[rune(skip)]
    shape: Shape,
}

/// Pair each of the query's notes with its high-water mark under
/// `key`: 1 when a task under the key processed the note at its
/// commit, 0 otherwise.
#[rune::function(instance, path = hwm)]
pub(crate) fn notes_hwm(notes: Ref<NotesQuery>, key: Value) -> NotesHwm {
    NotesHwm {
        query: notes.clone(),
        key,
        shape: Shape::Hwm,
    }
}

/// The query's notes with a high-water mark of 0 under `key`.
#[rune::function(instance, path = unseen)]
pub(crate) fn notes_unseen(notes: Ref<NotesQuery>, key: Value) -> NotesHwm {
    NotesHwm {
        query: notes.clone(),
        key,
        shape: Shape::Unseen,
    }
}

async fn do_notes_hwm(q: NotesHwm) -> Result<Result<Vec<Value>, Error>, VmError> {
    let key = match encode_key(&q.key) {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let notes = match fetch_notes(q.query).await? {
        Ok(notes) => notes,
        Err(e) => return Ok(Err(e)),
    };
    let ctx = current()?;
    let with_commit: Vec<(&str, Option<&str>)> = notes
        .iter()
        .map(|n| (n.id.as_str(), n.commit.as_deref()))
        .collect();
    let marks = hwm(&ctx, &key, &with_commit).await?;
    let mut out = Vec::with_capacity(notes.len());
    for (n, hwm) in notes.into_iter().zip(marks) {
        let value = match q.shape {
            Shape::Hwm => rune::to_value((n, hwm)),
            Shape::Unseen if hwm == 0 => rune::to_value(n),
            Shape::Unseen => continue,
        };
        out.push(value.map_err(VmError::from)?);
    }
    Ok(Ok(out))
}

/// The high-water mark under `key` of each object given as
/// `(id, commit)`, in order: the largest mark recorded by any live
/// scan at a commit on the object's chain, or 0 with none. An object
/// with no commit, such as a note staged by this scan, is 0.
async fn hwm(
    ctx: &ScanContext,
    key: &str,
    objects: &[(&str, Option<&str>)],
) -> Result<Vec<i64>, VmError> {
    if ctx.invalidate {
        tracing::info!(key, "hwm ignores watermarks: scan invalidates prior work");
        return Ok(vec![0; objects.len()]);
    }
    let ids: Vec<&str> = objects
        .iter()
        .filter(|(_, commit)| commit.is_some())
        .map(|(id, _)| *id)
        .collect();
    if ids.is_empty() {
        return Ok(vec![0; objects.len()]);
    }
    let marks = marks(ctx, key, &ids).await?;
    let store = ctx.store.lock().await;
    let mut out = Vec::with_capacity(objects.len());
    for (id, commit) in objects {
        let Some(commit) = commit else {
            out.push(0);
            continue;
        };
        let hwm = match marks.get(*id) {
            Some(recorded) => {
                let chain: HashSet<String> = chain(&store, commit)?.into_iter().collect();
                recorded
                    .iter()
                    .filter(|(sha, _)| chain.contains(sha))
                    .map(|(_, mark)| *mark)
                    .max()
                    .unwrap_or(0)
            }
            None => 0,
        };
        out.push(i64::try_from(hwm).unwrap());
    }
    Ok(out)
}

/// The `(commit, mark)` records under `key` per object id.
async fn marks(
    ctx: &ScanContext,
    key: &str,
    ids: &[&str],
) -> Result<HashMap<String, Vec<(String, u64)>>, VmError> {
    let sql = format!(
        "SELECT oid, commit, mark FROM scan_watermark \
         WHERE key = '{}' AND oid IN ({})",
        sql_str(key),
        id_list(ids)
    );
    let batches = run(ctx.query_context().await?, &sql).await?;
    let mut out: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for batch in &batches {
        let oids = string_column(batch, 0);
        let commits = string_column(batch, 1);
        let marks = batch
            .column(2)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .expect("mark is an unsigned integer column");
        for i in 0..batch.num_rows() {
            out.entry(oids.value(i).to_string())
                .or_default()
                .push((commits.value(i).to_string(), marks.value(i)));
        }
    }
    Ok(out)
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

/// Link into this scan every note whose carry-forward key is `key`
/// and whose target is one of the scan's sessions at a commit in that
/// session's chain. The note commits are appended to the staged
/// carried list, which apply writes as `notes_carried.link`. Returns
/// the number of notes newly linked; a note already carried by this
/// scan counts zero.
async fn do_carry_forward_notes(q: CarryForwardNotes) -> Result<Result<i64, Error>, VmError> {
    let key = match encode_key(&q.key) {
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
    let sessions = members(&ctx, false).await?;
    if sessions.is_empty() {
        return Ok(Ok(0));
    }
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let sql = format!(
        "SELECT t.note_commit, t.target_id, t.target_commit \
         FROM note n JOIN note_target_link t ON t.note_id = n.id \
         WHERE n.carry_forward_key = '{}' AND t.target_type = 'session' \
           AND t.target_id IN ({})",
        sql_str(&key),
        id_list(&ids)
    );
    let batches = run(ctx.query_context().await?, &sql).await?;
    let chains = chains(&ctx, &sessions).await?;
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
    let lines: String = new.iter().map(|c| format!("{c}\n")).collect();
    append(path, &lines).map_err(|e| VmError::panic(format!("carried notes: {e}")))?;
    Ok(new.len())
}

/// The commit chain of each session, as a set, keyed by session id.
async fn chains(
    ctx: &ScanContext,
    sessions: &[Session],
) -> Result<HashMap<String, HashSet<String>>, VmError> {
    let store = ctx.store.lock().await;
    let mut out = HashMap::with_capacity(sessions.len());
    for s in sessions {
        let chain: HashSet<String> = chain(&store, &s.commit)?.into_iter().collect();
        out.insert(s.id.clone(), chain);
    }
    Ok(out)
}

/// The commit chain of an object from `commit` back to its first
/// commit: `commit` first, then each `parent` in turn.
fn chain(store: &Store, commit: &str) -> Result<Vec<String>, VmError> {
    let mut out = vec![commit.to_string()];
    let mut sha = commit.to_string();
    loop {
        let header = store
            .read_header(&sha)
            .map_err(|e| VmError::panic(format!("read object commit {sha}: {e}")))?;
        match header.parent {
            Some(parent) => {
                out.push(parent.clone());
                sha = parent;
            }
            None => return Ok(out),
        }
    }
}

fn id_list(ids: &[&str]) -> String {
    ids.iter()
        .map(|id| format!("'{}'", sql_str(id)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

/// A key in storage form: a string as given, or a tuple of strings
/// and integers colon-joined. The result is a path component and a
/// table value, so it must be non-empty and hold no `/`.
pub(crate) fn encode_key(key: &Value) -> Result<String, Error> {
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
