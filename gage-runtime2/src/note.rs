//! `write_note(name, value)`: a note written to the scan's staging.
//!
//! The builder carries the name, the value, the target set by one of
//! the `for_session*` methods, and the metadata. Awaiting it validates
//! the target through the store, writes the note tree under the scan's
//! staging (`gage_store::NoteStore::stage`), and returns the [`Note`].
//! The runtime sets `author` to `task:<scanner>:<task>` and
//! `attrs.scan` to the running scan. Apply creates the object. Bad
//! input is `Error::Args`; a failure to reach staging or the store is
//! a VM error. See implementation-notes.md, "`write_note` runtime
//! function".

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_runtime::error::Error;
use gage_runtime::value::{json_to_value, value_to_json};
use gage_store::{NoteInput, NoteStore, NoteValue, StoreError};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Object, Protocol, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::OUTPUT_SINK;
use crate::scan::current;

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
    m.associated_function(&Protocol::INTO_FUTURE, |w: NoteWrite| async move {
        do_write_note(w).await
    })?;
    m.ty::<Note>()?;
    m.function_meta(Note::debug)?;
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
    }
}

impl NoteWrite {
    /// Target a whole session.
    #[rune::function(instance)]
    fn for_session(mut self, session: &str) -> Self {
        self.target = Some(SessionTarget {
            session: session.to_string(),
            lines: Lines::None,
        });
        self
    }

    /// Target one line of a session.
    #[rune::function(instance)]
    fn for_session_line(mut self, session: &str, line: Value) -> Self {
        self.target = Some(SessionTarget {
            session: session.to_string(),
            lines: Lines::One(line),
        });
        self
    }

    /// Target an inclusive line range of a session.
    #[rune::function(instance)]
    fn for_session_range(mut self, session: &str, start: Value, end: Value) -> Self {
        self.target = Some(SessionTarget {
            session: session.to_string(),
            lines: Lines::Range(start, end),
        });
        self
    }

    /// Target lines of a session: a list of lines, or one string in the
    /// selection grammar. An empty list or string targets the whole
    /// session.
    #[rune::function(instance)]
    fn for_session_lines(mut self, session: &str, lines: Value) -> Self {
        self.target = Some(SessionTarget {
            session: session.to_string(),
            lines: Lines::Spec(lines),
        });
        self
    }

    /// The writer's payload, an object stored and returned verbatim.
    #[rune::function(instance)]
    fn metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
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
    };
    let staged = {
        let store = ctx.store.lock().unwrap();
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

/// A string is stored as text; anything else as JSON.
fn note_value(v: &Value) -> Result<NoteValue, Error> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(NoteValue::Text(s.to_string()));
    }
    let json =
        value_to_json(v).map_err(|e| Error::Args(format!("value could not be serialized: {e}")))?;
    Ok(NoteValue::Json(json))
}

fn metadata_json(v: &Value) -> Result<serde_json::Value, Error> {
    let json = value_to_json(v)
        .map_err(|e| Error::Args(format!("metadata could not be serialized: {e}")))?;
    if !json.is_object() {
        return Err(Error::Args("metadata must be an object".into()));
    }
    Ok(json)
}
