//! `write_note(name, value)`: a note written to the scan's staging,
//! and `scan().notes()`: the scan's own notes read back.
//!
//! The builder carries the name, the value, the target set by one of
//! the `for_session*` methods, and the metadata. Each `for_session*`
//! method takes the session as a `Session` or an id string and keeps
//! the id. Awaiting the builder validates
//! the target through the store, writes the note tree under the scan's
//! staging (`gage_store::NoteStore::stage`), and returns the [`Note`].
//! The runtime sets `author` to `task:<scanner>:<task>` and
//! `attrs.scan` to the running scan. Apply creates the object. Bad
//! input is `Error::Args`; a failure to reach staging or the store is
//! a VM error. See implementation-notes.md, "`write_note` runtime
//! function".
//!
//! `scan().notes()` is a [`NotesQuery`]; awaiting it reads the notes
//! staged by this scan's tasks and the notes carried into it, and
//! nothing else, as the rethink design says a running scan sees.
//! `.name(name)` and `.names([...])` match names exactly. A task sees
//! every note its upstream tasks wrote because the runner releases it
//! only after they returned.
//!
//! A `DateTime` value, as a note value or inside metadata, is stored
//! as its RFC 3339 string.

use std::fs;
use std::io;

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_runtime::datetime::DateTime;
use gage_runtime::error::Error;
use gage_runtime::value::{json_to_value, value_to_json};
use gage_store::{NoteFull, NoteInput, NoteStore, NoteValue, StoreError};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Object, Protocol, Ref, Value, Vec as RuneVec, VmError};
use rune::{Any, ContextError, Module};

use crate::OUTPUT_SINK;
use crate::scan::{Scan, ScanContext, current, session_id};
use crate::validate::work_key;

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("write_note", write_note).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<NoteWrite>()?;
    m.function_meta(NoteWrite::for_session)?;
    m.function_meta(NoteWrite::for_session_line)?;
    m.function_meta(NoteWrite::for_session_range)?;
    m.function_meta(NoteWrite::for_session_lines)?;
    m.function_meta(NoteWrite::metadata)?;
    m.function_meta(NoteWrite::work_key)?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: NoteWrite| async move {
        do_write_note(w).await
    })?;
    m.ty::<Note>()?;
    m.function_meta(Note::debug)?;
    m.ty::<NotesQuery>()?;
    m.function_meta(notes)?;
    m.function_meta(NotesQuery::name)?;
    m.function_meta(NotesQuery::names)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NotesQuery| async move {
        fetch_notes(q).await
    })?;
    Ok(m)
}

/// The builder `write_note(name, value)` returns.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct NoteWrite {
    #[rune(skip)]
    name: String,
    #[rune(skip)]
    value: Value,
    /// The session target as given; rendered and validated at the await
    #[rune(skip)]
    target: Option<SessionTarget>,
    #[rune(skip)]
    metadata: Option<Value>,
    #[rune(skip)]
    work_key: Option<Value>,
}

/// A session target before rendering: the id and the lines as the
/// scanner passed them.
struct SessionTarget {
    session: String,
    lines: Lines,
}

enum Lines {
    None,
    One(Value),
    Range(Value, Value),
    Spec(Value),
}

fn write_note(name: &str, value: Value) -> NoteWrite {
    NoteWrite {
        name: name.to_string(),
        value,
        target: None,
        metadata: None,
        work_key: None,
    }
}

impl NoteWrite {
    /// Target a whole session. `session` is a `Session` or an id string.
    #[rune::function(instance)]
    fn for_session(mut self, session: Value) -> Result<Self, VmError> {
        self.target = Some(SessionTarget {
            session: session_id(&session)?,
            lines: Lines::None,
        });
        Ok(self)
    }

    /// Target one line of a session.
    #[rune::function(instance)]
    fn for_session_line(mut self, session: Value, line: Value) -> Result<Self, VmError> {
        self.target = Some(SessionTarget {
            session: session_id(&session)?,
            lines: Lines::One(line),
        });
        Ok(self)
    }

    /// Target an inclusive line range of a session.
    #[rune::function(instance)]
    fn for_session_range(
        mut self,
        session: Value,
        start: Value,
        end: Value,
    ) -> Result<Self, VmError> {
        self.target = Some(SessionTarget {
            session: session_id(&session)?,
            lines: Lines::Range(start, end),
        });
        Ok(self)
    }

    /// Target lines of a session: a list of lines, or one string in the
    /// selection grammar. An empty list or string targets the whole
    /// session.
    #[rune::function(instance)]
    fn for_session_lines(mut self, session: Value, lines: Value) -> Result<Self, VmError> {
        self.target = Some(SessionTarget {
            session: session_id(&session)?,
            lines: Lines::Spec(lines),
        });
        Ok(self)
    }

    /// The writer's payload, an object stored and returned verbatim.
    #[rune::function(instance)]
    fn metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set the note's work key: a later scan's
    /// `carry_forward_notes(key)` links it when its target session is
    /// in that scan. A string, or a tuple of strings and integers
    /// rendered colon-joined.
    #[rune::function(instance)]
    fn work_key(mut self, key: Value) -> Self {
        self.work_key = Some(key);
        self
    }
}

/// A note the scan wrote, as returned to the scanner.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Note {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: String,
    #[rune(get)]
    pub value: Value,
    #[rune(get)]
    pub author: String,
    /// The target URL as stored, or `None`
    #[rune(get)]
    pub target: Option<String>,
    /// The metadata object; empty when none was given
    #[rune(get)]
    pub metadata: Value,
    /// UNIX time millis
    #[rune(get)]
    pub created: i64,
}

impl Note {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Note {{ id: {:?}, name: {:?}, author: {:?}, target: {:?}, value: ",
            self.id, self.name, self.author, self.target
        )?;
        self.value.debug_fmt(f)?;
        write!(f, ", created: {} }}", self.created)?;
        Ok(())
    }
}

/// The outer error is a VM error; the inner is the scanner's `Result`.
type Written = Result<Result<Note, Error>, VmError>;

async fn do_write_note(w: NoteWrite) -> Written {
    let ctx = current()?;
    let (scanner, task) = OUTPUT_SINK
        .try_with(|sink| (sink.scanner.clone(), sink.task.clone()))
        .map_err(|_outside_task| {
            VmError::panic("write_note is available only inside a running scan task")
        })?;
    let author = format!("task:{scanner}:{task}");

    let (target, session) = match &w.target {
        Some(t) => match render_target(t) {
            Ok(url) => (Some(url), Some(t.session.clone())),
            Err(e) => return Ok(Err(e)),
        },
        None => (None, None),
    };
    let value = match note_value(&w.value) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let metadata = match w.metadata.as_ref().map(metadata_json).transpose() {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };
    let key = match w.work_key.as_ref().map(work_key).transpose() {
        Ok(k) => k,
        Err(e) => return Ok(Err(e)),
    };
    let pinned = match &session {
        Some(id) => ctx.member_commit(id).await?,
        None => None,
    };

    let id = new_uuid();
    let input = NoteInput {
        name: &w.name,
        value,
        author: &author,
        target: target.as_deref(),
        metadata: metadata.clone(),
        work_key: key.as_deref(),
    };
    let staged = {
        let store = ctx.store.lock().await;
        NoteStore::from(&*store).stage(
            &ctx.paths.notes_dir.join(&id),
            &id,
            &input,
            &ctx.scan_id,
            pinned.as_deref(),
        )
    };
    match staged {
        Ok(()) => {}
        // A target the scanner named wrong is its error; anything else
        // is the store's
        Err(
            e @ (StoreError::BadUrl(_)
            | StoreError::BadLineSelection(_)
            | StoreError::BadTarget(_)
            | StoreError::TargetNotFound(_)
            | StoreError::ObjectDeleted(_)
            | StoreError::WrongType { .. }),
        ) => return Ok(Err(Error::Args(format!("write_note target: {e}")))),
        Err(e) => return Err(VmError::panic(format!("write_note: {e}"))),
    }
    tracing::debug!(id, name = w.name, author, target, "write_note");

    let metadata = match metadata {
        Some(json) => json_to_value(&json),
        None => rune::to_value(Object::new()).map_err(VmError::from)?,
    };
    Ok(Ok(Note {
        id,
        name: w.name,
        value: w.value,
        author,
        target,
        metadata,
        created: now_ms(),
    }))
}

/// The stored target URL for a session target.
fn render_target(t: &SessionTarget) -> Result<String, Error> {
    let fragment = match &t.lines {
        Lines::None => String::new(),
        Lines::One(line) => line_arg(line)?.to_string(),
        Lines::Range(start, end) => format!("{}-{}", line_arg(start)?, line_arg(end)?),
        Lines::Spec(spec) => lines_spec(spec)?,
    };
    if fragment.is_empty() {
        Ok(format!("session:{}", t.session))
    } else {
        Ok(format!("session:{}#{fragment}", t.session))
    }
}

/// A line argument: an integer, or a string holding one.
fn line_arg(v: &Value) -> Result<u64, Error> {
    if let Ok(n) = v.as_integer::<i64>() {
        return u64::try_from(n)
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| Error::Args(format!("line must be 1 or greater, got {n}")));
    }
    if let Ok(s) = v.borrow_string_ref() {
        return s
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| Error::Args(format!("line must be a positive integer, got {s:?}")));
    }
    Err(Error::Args("line must be an integer or a string".into()))
}

/// The fragment for `for_session_lines`: a list of lines joined by
/// `,`, or a selection string as given.
fn lines_spec(v: &Value) -> Result<String, Error> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(s.trim().to_string());
    }
    if let Ok(list) = v.borrow_ref::<rune::runtime::Vec>() {
        let mut parts = Vec::with_capacity(list.len());
        for item in list.iter() {
            parts.push(line_arg(item)?.to_string());
        }
        return Ok(parts.join(","));
    }
    Err(Error::Args(
        "lines must be a list of lines or a selection string".into(),
    ))
}

/// A string is stored as text, a `DateTime` as its RFC 3339 string,
/// anything else as JSON.
fn note_value(v: &Value) -> Result<NoteValue, Error> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(NoteValue::Text(s.to_string()));
    }
    if let Ok(dt) = v.borrow_ref::<DateTime>() {
        return Ok(NoteValue::Text(dt.to_rfc3339()));
    }
    let json =
        json_value(v).map_err(|e| Error::Args(format!("value could not be serialized: {e}")))?;
    Ok(NoteValue::Json(json))
}

fn metadata_json(v: &Value) -> Result<serde_json::Value, Error> {
    let json =
        json_value(v).map_err(|e| Error::Args(format!("metadata could not be serialized: {e}")))?;
    if !json.is_object() {
        return Err(Error::Args("metadata must be an object".into()));
    }
    Ok(json)
}

/// Encode a value as JSON with a `DateTime` as its RFC 3339 string,
/// at the top or anywhere inside an object or list. Everything else
/// is the first generation's encoding.
fn json_value(v: &Value) -> Result<serde_json::Value, String> {
    if let Ok(dt) = v.borrow_ref::<DateTime>() {
        return Ok(serde_json::Value::String(dt.to_rfc3339()));
    }
    if let Ok(obj) = v.borrow_ref::<Object>() {
        let mut map = serde_json::Map::with_capacity(obj.len());
        for (k, val) in obj.iter() {
            map.insert(k.as_str().to_owned(), json_value(val)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    if let Ok(list) = v.borrow_ref::<RuneVec>() {
        let mut out = Vec::with_capacity(list.len());
        for val in list.iter() {
            out.push(json_value(val)?);
        }
        return Ok(serde_json::Value::Array(out));
    }
    value_to_json(v)
}

/// The value of `scan().notes()`: the scan's own notes, read when
/// awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct NotesQuery {
    /// Exact names to keep; `None` keeps every note
    #[rune(skip)]
    names: Option<Vec<String>>,
}

/// The notes this scan wrote or carried, read when awaited.
#[rune::function(instance)]
fn notes(_scan: Ref<Scan>) -> NotesQuery {
    NotesQuery { names: None }
}

impl NotesQuery {
    /// Keep the notes named `name`.
    #[rune::function(instance)]
    fn name(mut self, name: &str) -> Self {
        self.names = Some(vec![name.to_string()]);
        self
    }

    /// Keep the notes with any of `names`.
    #[rune::function(instance)]
    fn names(mut self, names: Ref<RuneVec>) -> Result<Self, VmError> {
        let mut out = Vec::with_capacity(names.len());
        for v in names.iter() {
            let s = v
                .borrow_string_ref()
                .map_err(|e| VmError::panic(format!("names: expected strings: {e}")))?;
            out.push(s.to_string());
        }
        self.names = Some(out);
        Ok(self)
    }
}

/// Read the staged and carried notes, filtered by name, oldest first
/// and by id among equals.
async fn fetch_notes(q: NotesQuery) -> Result<Result<Vec<Note>, Error>, VmError> {
    let ctx = current()?;
    let mut full = staged_notes(&ctx).await?;
    full.extend(carried_notes(&ctx).await?);
    full.retain(|n| q.names.as_ref().is_none_or(|names| names.contains(&n.name)));
    full.sort_by(|a, b| {
        a.created_ms
            .cmp(&b.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out = Vec::with_capacity(full.len());
    for n in full {
        out.push(note_from_full(n)?);
    }
    Ok(Ok(out))
}

/// The notes staged under the scan's notes directory, in id order.
async fn staged_notes(ctx: &ScanContext) -> Result<Vec<NoteFull>, VmError> {
    let entries = match fs::read_dir(&ctx.paths.notes_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(VmError::panic(format!("staged notes: {e}"))),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| VmError::panic(format!("staged notes: {e}")))?
            .path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    let store = ctx.store.lock().await;
    let notes = NoteStore::from(&*store);
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs {
        out.push(
            notes
                .read_staged(&dir)
                .map_err(|e| VmError::panic(format!("staged note {}: {e}", dir.display())))?,
        );
    }
    Ok(out)
}

/// The notes carried into the scan so far, in the order they were
/// carried.
async fn carried_notes(ctx: &ScanContext) -> Result<Vec<NoteFull>, VmError> {
    let text = match fs::read_to_string(&ctx.paths.carried_notes) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(VmError::panic(format!("carried notes: {e}"))),
    };
    let store = ctx.store.lock().await;
    let notes = NoteStore::from(&*store);
    let mut out = Vec::new();
    for sha in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        out.push(
            notes
                .at_commit(sha)
                .map_err(|e| VmError::panic(format!("carried note {sha}: {e}")))?,
        );
    }
    Ok(out)
}

fn note_from_full(n: NoteFull) -> Result<Note, VmError> {
    let value = match n.value {
        NoteValue::Text(s) => rune::to_value(s).map_err(VmError::from)?,
        NoteValue::Json(json) => json_to_value(&json),
    };
    let metadata = match n.metadata {
        Some(json) => json_to_value(&json),
        None => rune::to_value(Object::new()).map_err(VmError::from)?,
    };
    Ok(Note {
        id: n.id,
        name: n.name,
        value,
        author: n.author,
        target: n.target,
        metadata,
        created: n.created_ms,
    })
}

#[cfg(test)]
mod tests {
    use rune::Vm;
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources};

    use super::*;

    fn vm(script: &str) -> Vm {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        Vm::new(rt, RuneArc::try_new(unit).unwrap())
    }

    /// `names` borrows its list, so the caller's list is still
    /// readable afterwards.
    #[test]
    fn notes_names_leaves_the_caller_list_readable() {
        let mut vm = vm(r#"
            pub fn check(scan) {
                let names = ["a", "b"];
                let query = scan.notes().names(names);
                (names.len(), names[1])
            }
            "#);
        let scan = Scan {
            id: "scan".into(),
            dataset: None,
        };
        let output = vm.call(["check"], (scan,)).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (len, second): (i64, String) = rune::from_value(output).unwrap();
        assert_eq!((len, second.as_str()), (2, "b"));
    }
}
