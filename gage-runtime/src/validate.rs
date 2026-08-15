//! Task-run memoization: `split_valid` / `files_valid` /
//! `carry_forward_notes`.
//!
//! Partitions a scan's sessions or notes by validation state recorded
//! in `task_validate` so scanners can skip work already done under the
//! same key, adopt the notes a prior scan produced, and record
//! validation state when the work completes. See
//! `.local.design/session-caching.md`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gage_db::note;
use gage_db::scan::insert_scan_note;
use gage_db::task_validate::{self, note_ref, project_ref, session_ref};
use rune::runtime::{Function, Protocol, Ref, Value, Vec as RuneVec};
use rune::{Any, ContextError, Module};

use crate::config::{Config, Project};
use crate::db::{Note, NotesQuery, fetch_notes};
use crate::error::Error;
use crate::scan::{Session, Sessions};
use crate::state::current_scan_ctx;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function_meta(sessions_split_valid)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionsSplitValid| async move {
        do_sessions_split_valid(q)
    })?;

    m.function_meta(notes_split_valid)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NotesSplitValid| async move {
        do_notes_split_valid(q)
    })?;

    m.function("files_valid", FilesValid::new).build()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: FilesValid| async move {
        do_files_valid(q)
    })?;

    m.function("carry_forward_notes", CarryForward::new)
        .build()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: CarryForward| async move {
        do_carry_forward(q)
    })?;
    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<SessionsSplitValid>()?;
    m.ty::<NotesSplitValid>()?;
    m.ty::<FilesValid>()?;
    m.ty::<CarryForward>()?;
    Ok(())
}

/// Builder returned by `sessions.split_valid(key)`. The partition runs
/// when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct SessionsSplitValid {
    #[rune(skip)]
    sessions: Vec<Session>,
    #[rune(skip)]
    key: Value,
}

#[rune::function(instance, path = split_valid)]
fn sessions_split_valid(sessions: Ref<Sessions>, key: Value) -> SessionsSplitValid {
    SessionsSplitValid {
        sessions: sessions.remaining(),
        key,
    }
}

/// Render a task validation key — a tuple/vec of strings and integers —
/// as its colon-joined form, e.g. `("s", "findings", 1)` →
/// `"s:findings:1"`.
fn key_string(key: &Value) -> crate::Result<String> {
    let json = crate::value::value_to_json(key)
        .map_err(|e| Error::Args(format!("validation key could not be serialized: {e}")))?;
    let parts = match json {
        serde_json::Value::Array(items) => items,
        _ => return Err(Error::Args("validation key must be a tuple or vec".into())),
    };
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            serde_json::Value::String(s) => out.push(s),
            serde_json::Value::Number(n) => out.push(n.to_string()),
            other => {
                return Err(Error::Args(format!(
                    "validation key elements must be strings or integers, got {other}"
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::Args("validation key must not be empty".into()));
    }
    Ok(out.join(":"))
}

/// Partition sessions by recorded size validator. Returns
/// `(prev, new, validate)`: `prev` are sessions whose recorded size
/// matches (already processed), `new` need scanning, and `validate` is
/// a function of one session (or session id) that records the session's
/// size under the key — called by the scanner when that session's work
/// has completed.
fn do_sessions_split_valid(
    q: SessionsSplitValid,
) -> crate::Result<(Vec<Session>, Vec<Session>, Function)> {
    let key = key_string(&q.key)?;
    let ctx = current_scan_ctx();
    let sizes: HashMap<&str, u64> = ctx
        .run
        .selected
        .iter()
        .map(|s| (s.id.as_str(), s.size))
        .collect();

    let mut prev = Vec::new();
    let mut new = Vec::new();
    // session id -> size at selection, for `validate`
    let mut entries: HashMap<String, String> = HashMap::new();
    {
        let db = ctx.db.lock().unwrap();
        for s in q.sessions {
            let size = *sizes
                .get(s.id.as_str())
                .ok_or_else(|| Error::Args(format!("session {} not in scan selection", s.id)))?;
            let recorded = task_validate::value(&db, &key, &session_ref(&s.id))
                .map_err(|e| Error::Db(e.to_string()))?;
            if recorded.as_deref() == Some(size.to_string().as_str()) {
                prev.push(s);
            } else {
                entries.insert(s.id.clone(), size.to_string());
                new.push(s);
            }
        }
    }

    let validate = Function::new(move |session: Value| {
        let key = key.clone();
        let entries = entries.clone();
        async move {
            let id = session_id(&session)?;
            let size = entries
                .get(&id)
                .ok_or_else(|| Error::Args(format!("session {id} was not split as new")))?;
            let ctx = current_scan_ctx();
            let db = ctx.db.lock().unwrap();
            task_validate::put(&db, &key, &session_ref(&id), Some(size))
                .map_err(|e| Error::Db(e.to_string()))
        }
    })
    .unwrap();

    Ok((prev, new, validate))
}

/// A `Session` or a session id string.
fn session_id(v: &Value) -> crate::Result<String> {
    if let Ok(s) = rune::from_value::<Ref<Session>>(v.clone()) {
        return Ok(s.id.clone());
    }
    v.borrow_string_ref().map(|s| s.to_string()).map_err(|e| {
        Error::Args(format!(
            "expected a Session or session id string, got {}: {e}",
            v.type_info()
        ))
    })
}

/// Builder returned by `notes_query.split_valid(key)`. The partition
/// runs when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct NotesSplitValid {
    #[rune(skip)]
    query: NotesQuery,
    #[rune(skip)]
    key: Value,
}

#[rune::function(instance, path = split_valid)]
fn notes_split_valid(query: NotesQuery, key: Value) -> NotesSplitValid {
    NotesSplitValid { query, key }
}

/// Partition the query's notes by membership under the key. Returns
/// `(prev, new, validate)`: `prev` were recorded by a prior
/// `validate()` call, `new` have never been recorded, and `validate`
/// is a no-argument function that records the `new` notes — called by
/// the scanner when the work consuming them has completed.
fn do_notes_split_valid(q: NotesSplitValid) -> crate::Result<(Vec<Note>, Vec<Note>, Function)> {
    let key = key_string(&q.key)?;
    let ctx = current_scan_ctx();
    let notes = fetch_notes(q.query)?;

    let refs: Vec<String> = notes.iter().map(|n| note_ref(&n.id)).collect();
    let existing: HashSet<String> = {
        let db = ctx.db.lock().unwrap();
        task_validate::existing_refs(&db, &key, &refs)
            .map_err(|e| Error::Db(e.to_string()))?
            .into_iter()
            .collect()
    };

    let mut prev = Vec::new();
    let mut new = Vec::new();
    let mut new_refs = Vec::new();
    for note in notes {
        let r = note_ref(&note.id);
        if existing.contains(&r) {
            prev.push(note);
        } else {
            new_refs.push(r);
            new.push(note);
        }
    }

    let validate = Function::new(move || {
        let key = key.clone();
        let new_refs = new_refs.clone();
        async move {
            let ctx = current_scan_ctx();
            let db = ctx.db.lock().unwrap();
            for r in &new_refs {
                task_validate::put(&db, &key, r, None).map_err(|e| Error::Db(e.to_string()))?;
            }
            Ok::<(), Error>(())
        }
    })
    .unwrap();

    Ok((prev, new, validate))
}

/// Builder returned by `files_valid(key, project, files)`. The check
/// runs when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct FilesValid {
    #[rune(skip)]
    key: Value,
    #[rune(skip)]
    project: Value,
    #[rune(skip)]
    files: Value,
}

impl FilesValid {
    fn new(key: Value, project: Value, files: Value) -> Self {
        Self {
            key,
            project,
            files,
        }
    }
}

/// Check a project's file set against the digest recorded under the
/// key. Returns `(valid, validate)`: `valid` is true when the recorded
/// digest matches the current file set, and `validate` is a
/// no-argument function that records the current digest — called by
/// the scanner when the work consuming the files has completed.
fn do_files_valid(q: FilesValid) -> crate::Result<(bool, Function)> {
    let key = key_string(&q.key)?;
    let ref_ = project_ref(&project_path(&q.project)?);
    let digest = files_digest(&config_paths(&q.files)?)?;

    let ctx = current_scan_ctx();
    let valid = {
        let db = ctx.db.lock().unwrap();
        task_validate::value(&db, &key, &ref_)
            .map_err(|e| Error::Db(e.to_string()))?
            .as_deref()
            == Some(digest.as_str())
    };

    let validate = Function::new(move || {
        let key = key.clone();
        let ref_ = ref_.clone();
        let digest = digest.clone();
        async move {
            let ctx = current_scan_ctx();
            let db = ctx.db.lock().unwrap();
            task_validate::put(&db, &key, &ref_, Some(&digest))
                .map_err(|e| Error::Db(e.to_string()))
        }
    })
    .unwrap();

    Ok((valid, validate))
}

/// A `Project` or a project path string.
fn project_path(v: &Value) -> crate::Result<String> {
    if let Ok(p) = rune::from_value::<Ref<Project>>(v.clone()) {
        return Ok(p.path.to_string_lossy().into_owned());
    }
    v.borrow_string_ref().map(|s| s.to_string()).map_err(|e| {
        Error::Args(format!(
            "expected a Project or project path string, got {}: {e}",
            v.type_info()
        ))
    })
}

/// Paths of a list of `Config` rows.
fn config_paths(v: &Value) -> crate::Result<Vec<PathBuf>> {
    let items: RuneVec = rune::from_value(v.clone())
        .map_err(|e| Error::Args(format!("'files' must be a list of Config rows: {e}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        let c = rune::from_value::<Ref<Config>>(item.clone())
            .map_err(|e| Error::Args(format!("'files' entries must be Config rows: {e}")))?;
        out.push(c.path.clone());
    }
    Ok(out)
}

/// Deterministic digest over the file set: sorted `(path, content)`
/// pairs folded through the std hasher. Any file addition, deletion,
/// or content change produces a different digest. The std hasher is
/// not guaranteed stable across Rust releases; a hasher change costs
/// one redundant re-run per project, not correctness.
fn files_digest(paths: &[PathBuf]) -> crate::Result<String> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut sorted: Vec<&PathBuf> = paths.iter().collect();
    sorted.sort();
    let mut h = DefaultHasher::new();
    for p in sorted {
        p.hash(&mut h);
        let content =
            std::fs::read(p).map_err(|e| Error::Config(format!("read {}: {e}", p.display())))?;
        content.hash(&mut h);
    }
    Ok(format!("{:016x}", h.finish()))
}

/// Builder returned by `carry_forward_notes(sessions, names)`. The link
/// runs when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CarryForward {
    #[rune(skip)]
    sessions: Value,
    #[rune(skip)]
    names: Value,
}

impl CarryForward {
    fn new(sessions: Value, names: Value) -> Self {
        Self { sessions, names }
    }
}

/// Link each session's prior notes matching `names` (exactly) into the
/// current scan's `scan_note` rows, making them visible to downstream
/// tasks as if written by this scan. Returns the number of notes
/// linked.
fn do_carry_forward(q: CarryForward) -> crate::Result<i64> {
    let items: RuneVec = rune::from_value(q.sessions)
        .map_err(|e| Error::Args(format!("'sessions' must be a list: {e}")))?;
    let mut session_ids = Vec::with_capacity(items.len());
    for item in items.iter() {
        session_ids.push(session_id(item)?);
    }

    let items: RuneVec = rune::from_value(q.names)
        .map_err(|e| Error::Args(format!("'names' must be a list: {e}")))?;
    let mut names = Vec::with_capacity(items.len());
    for item in items.iter() {
        let name = item
            .borrow_string_ref()
            .map_err(|e| Error::Args(format!("'names' entries must be strings: {e}")))?;
        names.push(name.to_string());
    }

    let ctx = current_scan_ctx();
    let db = ctx.db.lock().unwrap();
    let mut count = 0i64;
    for sid in &session_ids {
        for name in &names {
            let ids = note::ids_for_session_by_name(&db, sid, name)
                .map_err(|e| Error::Db(e.to_string()))?;
            for id in &ids {
                insert_scan_note(&db, &ctx.run.scan_id, id)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }
            count += i64::try_from(ids.len()).unwrap();
        }
    }
    Ok(count)
}
