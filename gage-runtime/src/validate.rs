//! Task-run memoization: `split_valid` / `split_valid_range` /
//! `files_valid` / `carry_forward_notes`.
//!
//! Partitions a scan's sessions or notes by validation state recorded
//! in `task_validate` so scanners can skip work already done under the
//! same key, adopt the notes a prior scan produced, and record
//! validation state when the work completes. See
//! `.local.design/session-caching.md`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gage_db::note;
use gage_db::scan::{ScanLinkRole, insert_scan_note};
use gage_db::target::NoteTarget;
use gage_db::task_validate::{self, note_ref, project_ref, session_ref};
use rune::runtime::{Function, Protocol, Ref, Value, Vec as RuneVec};
use rune::{Any, ContextError, Module};

use crate::config::{Config, Project};
use crate::db::{Note, NotesQuery, fetch_notes, target_from_value};
use crate::error::Error;
use crate::scan::{Range, Session, Sessions};
use crate::state::current_scan_ctx;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function_meta(sessions_split_valid)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionsSplitValid| async move {
        do_sessions_split_valid(q)
    })?;

    m.function_meta(sessions_split_valid_range)?;
    m.associated_function(
        &Protocol::INTO_FUTURE,
        |q: SessionsSplitValidRange| async move { do_sessions_split_valid_range(q) },
    )?;

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
    m.ty::<SessionsSplitValidRange>()?;
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
    let (prev, new, entries, _) = split_by_size(q.sessions, &key)?;
    let validate = size_validate_fn(key, entries);
    Ok((prev, new, validate))
}

/// Partition `sessions` by comparing each one's size at selection
/// against its recorded size under `key`. Returns the valid and stale
/// partitions, the stale sessions' sizes for `validate`
/// (session id -> size string), and the stale sessions' recorded
/// sizes where one exists (session id -> recorded size).
type SizeSplit = (
    Vec<Session>,
    Vec<Session>,
    HashMap<String, String>,
    HashMap<String, u64>,
);

fn split_by_size(sessions: Vec<Session>, key: &str) -> crate::Result<SizeSplit> {
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
    let mut recorded_sizes: HashMap<String, u64> = HashMap::new();
    let db = ctx.db.lock().unwrap();
    for s in sessions {
        let size = *sizes
            .get(s.id.as_str())
            .ok_or_else(|| Error::Args(format!("session {} not in scan selection", s.id)))?;
        if ctx.run.invalidate {
            // Deleted rather than bypassed: an interrupted run must
            // not leave the old row to validate a later scan
            task_validate::delete(&db, key, &session_ref(&s.id))
                .map_err(|e| Error::Db(e.to_string()))?;
            entries.insert(s.id.clone(), size.to_string());
            new.push(s);
            continue;
        }
        let recorded = task_validate::value(&db, key, &session_ref(&s.id))
            .map_err(|e| Error::Db(e.to_string()))?;
        if recorded.as_deref() == Some(size.to_string().as_str()) {
            prev.push(s);
        } else {
            if let Some(r) = recorded.and_then(|v| v.parse::<u64>().ok()) {
                recorded_sizes.insert(s.id.clone(), r);
            }
            entries.insert(s.id.clone(), size.to_string());
            new.push(s);
        }
    }
    Ok((prev, new, entries, recorded_sizes))
}

/// The deferred validate for the size validator: called with a session
/// (or session id) split as stale, records its size at selection.
fn size_validate_fn(key: String, entries: HashMap<String, String>) -> Function {
    Function::new(move |session: Value| {
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
    .unwrap()
}

/// Builder returned by `sessions.split_valid_range(key)`. The
/// partition runs when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct SessionsSplitValidRange {
    #[rune(skip)]
    sessions: Vec<Session>,
    #[rune(skip)]
    key: Value,
}

#[rune::function(instance, path = split_valid_range)]
fn sessions_split_valid_range(sessions: Ref<Sessions>, key: Value) -> SessionsSplitValidRange {
    SessionsSplitValidRange {
        sessions: sessions.remaining(),
        key,
    }
}

/// Partition sessions by recorded size, exactly as `split_valid` does,
/// and give each grown stale session its unseen line range. Returns
/// `(prev, new, validate)` over plain `Session` values: sessions in
/// `prev` and unseen sessions in `new` have `range: None`; a grown
/// session in `new` has `Some(Range)` spanning its unscanned lines.
fn do_sessions_split_valid_range(
    q: SessionsSplitValidRange,
) -> crate::Result<(Vec<Session>, Vec<Session>, Function)> {
    let key = key_string(&q.key)?;
    let (prev, mut new, entries, recorded_sizes) = split_by_size(q.sessions, &key)?;

    for s in &mut new {
        let Some(&recorded) = recorded_sizes.get(&s.id) else {
            continue;
        };
        let Some(size) = entries.get(&s.id).and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        if recorded >= size {
            // Not grown (shrunk or replaced): re-scan whole
            continue;
        }
        s.range = derive_range(&s.src, recorded, size)?.map(|(start, end)| Range { start, end });
    }

    let validate = size_validate_fn(key, entries);
    Ok((prev, new, validate))
}

/// The unseen line range of a session that grew from `recorded` to
/// `size` bytes. Session files are append-only JSONL, one entry per
/// newline-terminated line: the newline count over the first
/// `recorded` bytes is the validated line count, and the count over
/// the first `size` bytes is the current line count (a trailing
/// partial line counts as a line). Returns inclusive 1-based bounds,
/// or `None` when the file no longer supports a sane derivation.
fn derive_range(path: &Path, recorded: u64, size: u64) -> crate::Result<Option<(u64, u64)>> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|e| Error::Config(format!("read session {}: {e}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; 64 * 1024];
    let mut pos: u64 = 0;
    let mut validated_lines: u64 = 0;
    let mut lines: u64 = 0;
    let mut last_newline_end: u64 = 0;
    while pos < size {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::Config(format!("read session {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        // Count only up to the size at selection; content appended
        // since is next scan's business
        let used = usize::try_from((size - pos).min(n as u64)).unwrap();
        for (i, b) in buf.iter().take(used).enumerate() {
            if *b == b'\n' {
                lines += 1;
                let end = pos + i as u64 + 1;
                last_newline_end = end;
                if end <= recorded {
                    validated_lines += 1;
                }
            }
        }
        pos += used as u64;
    }
    // A trailing partial line is a line
    let current_lines = if pos > last_newline_end {
        lines + 1
    } else {
        lines
    };
    let start = validated_lines + 1;
    if current_lines < start {
        return Ok(None);
    }
    Ok(Some((start, current_lines)))
}

/// A `Session` or a session id string.
fn session_id(v: &Value) -> crate::Result<String> {
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
        if ctx.run.invalidate {
            for r in &refs {
                task_validate::delete(&db, &key, r).map_err(|e| Error::Db(e.to_string()))?;
            }
            HashSet::new()
        } else {
            task_validate::existing_refs(&db, &key, &refs)
                .map_err(|e| Error::Db(e.to_string()))?
                .into_iter()
                .collect()
        }
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
        if ctx.run.invalidate {
            task_validate::delete(&db, &key, &ref_).map_err(|e| Error::Db(e.to_string()))?;
            false
        } else {
            task_validate::value(&db, &key, &ref_)
                .map_err(|e| Error::Db(e.to_string()))?
                .as_deref()
                == Some(digest.as_str())
        }
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
    if let Ok(p) = v.borrow_ref::<Project>() {
        return Ok(p.path.to_string_lossy().into_owned());
    }
    v.borrow_string_ref().map(|s| s.to_string()).map_err(|e| {
        Error::Args(format!(
            "expected a Project or project path string, got {}: {e}",
            v.type_info()
        ))
    })
}

/// Paths of a list of `Config` rows. Borrows the list — a take would
/// gut the caller's value, which scanners reuse after the call.
fn config_paths(v: &Value) -> crate::Result<Vec<PathBuf>> {
    let items = v
        .borrow_ref::<RuneVec>()
        .map_err(|e| Error::Args(format!("'files' must be a list of Config rows: {e}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        let c = item
            .borrow_ref::<Config>()
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

/// Builder returned by `carry_forward_notes(targets, names)`. The link
/// runs when the value is awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct CarryForward {
    #[rune(skip)]
    targets: Value,
    #[rune(skip)]
    names: Value,
}

impl CarryForward {
    fn new(targets: Value, names: Value) -> Self {
        Self { targets, names }
    }
}

/// Link each target's prior notes matching `names` (exactly) into the
/// current scan's `scan_note` rows, making them visible to downstream
/// tasks as if written by this scan. Returns the number of notes
/// linked.
fn do_carry_forward(q: CarryForward) -> crate::Result<i64> {
    let items = q
        .targets
        .borrow_ref::<RuneVec>()
        .map_err(|e| Error::Args(format!("'targets' must be a list: {e}")))?;
    let mut anchors = Vec::with_capacity(items.len());
    for item in items.iter() {
        anchors.push(anchor(item)?);
    }

    let items = q
        .names
        .borrow_ref::<RuneVec>()
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
    for a in &anchors {
        for name in &names {
            let ids = match a {
                Anchor::Session(id) => note::ids_for_session_by_name(&db, id, name),
                Anchor::Project(path) => note::ids_for_project_by_name(&db, path, name),
            }
            .map_err(|e| Error::Db(e.to_string()))?;
            for id in &ids {
                insert_scan_note(&db, &ctx.run.scan_id, id, ScanLinkRole::Carried)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }
            count += i64::try_from(ids.len()).unwrap();
        }
    }
    Ok(count)
}

/// What a carry-forward entry anchors to: the session or project a
/// prior note targets.
enum Anchor {
    Session(String),
    Project(String),
}

/// A `Session`, a `Project`, or a target object (`#{ session: id }` /
/// `#{ project: path }`), the same target shape `write_note` accepts.
fn anchor(v: &Value) -> crate::Result<Anchor> {
    if let Ok(s) = v.borrow_ref::<Session>() {
        return Ok(Anchor::Session(s.id.clone()));
    }
    if let Ok(p) = v.borrow_ref::<Project>() {
        return Ok(Anchor::Project(p.path.to_string_lossy().into_owned()));
    }
    let target = target_from_value(v).map_err(|e| {
        Error::Args(format!(
            "expected a Session, Project, or target object: {e}"
        ))
    })?;
    match target {
        NoteTarget::Session(t) if t.line.is_some() => Err(Error::Args(
            "carry-forward session target does not accept 'line'".into(),
        )),
        NoteTarget::Session(t) => Ok(Anchor::Session(t.session_id)),
        NoteTarget::Project(t) => Ok(Anchor::Project(t.project_path)),
        NoteTarget::Scan(_) => Err(Error::Args(
            "carry-forward target must be a session or project".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::derive_range;
    use std::io::Write;

    fn session_file(content: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f
    }

    #[test]
    fn grown_by_whole_lines() {
        // Two validated lines, one appended
        let f = session_file(b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
        let recorded = 16; // through the second newline
        let size = 24;
        assert_eq!(
            derive_range(f.path(), recorded, size).unwrap(),
            Some((3, 3))
        );
    }

    #[test]
    fn trailing_partial_line_counts() {
        let f = session_file(b"{\"a\":1}\n{\"b\":2}\n{\"c\"");
        let recorded = 16;
        let size = 20;
        assert_eq!(
            derive_range(f.path(), recorded, size).unwrap(),
            Some((3, 3))
        );
    }

    #[test]
    fn recorded_bisecting_a_line_reincludes_it() {
        // Validation caught line 2 mid-append: only line 1 counts as
        // validated, so line 2 is re-scanned
        let f = session_file(b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
        let recorded = 12; // inside the second line
        let size = 24;
        assert_eq!(
            derive_range(f.path(), recorded, size).unwrap(),
            Some((2, 3))
        );
    }

    #[test]
    fn file_shorter_than_recorded_is_not_ranged() {
        let f = session_file(b"{\"a\":1}\n");
        assert_eq!(derive_range(f.path(), 16, 24).unwrap(), None);
    }

    #[test]
    fn size_caps_the_count() {
        // Lines appended after selection (beyond `size`) are excluded
        let f = session_file(b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n{\"d\":4}\n");
        let recorded = 8;
        let size = 16; // selection saw two lines
        assert_eq!(
            derive_range(f.path(), recorded, size).unwrap(),
            Some((2, 2))
        );
    }
}
