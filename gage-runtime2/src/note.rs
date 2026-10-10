//! `write_note(name, value)`: a note written to the scan directory,
//! and `scan().notes()`: the scan's own notes read back.
//!
//! The builder carries the name, the value, the target set by
//! `.target(object)`, and the metadata. The target is a `Target`, or
//! a `Session` or an `Attachment` as a whole-object target. Awaiting
//! the builder validates the target through the store, writes the
//! note tree into the scan directory
//! (`gage_store::NoteStore::write_to_dir`) linked to the version of
//! the target the scan reads, and returns the [`Note`]. The runtime sets `author` to `task:<scanner>:<task>` and
//! `attrs.scan` to the active scan. Apply creates the object. Bad
//! input is `Error::Args`; a failure to reach the scan directory or
//! the store is a VM error.
//!
//! `scan().notes()` is a [`NotesQuery`]; awaiting it reads the notes
//! written by this scan's tasks and the notes carried into it, and
//! nothing else, through the scan-scoped `note` table. It takes no
//! argument, so its await is the list itself. `.name(name)` and
//! `.names([...])` match names exactly or by a `*` pattern and move
//! the chain to a [`FilteredNotesQuery`], whose await is a `Result`:
//! a pattern is a syntax, and the scanner handles a bad one. A task
//! sees every note its upstream tasks wrote because the runner
//! releases it only after they returned. `.hwm(key)` and
//! `.unseen(key)` on either query read the notes' watermarks under
//! `key`; see `crate::validate`.
//!
//! A `DateTime` value, as a note value or inside metadata, is stored
//! as its RFC 3339 string.

use datafusion::arrow::array::{Array, StringArray, TimestampMillisecondArray};
use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_runtime::datetime::DateTime;
use gage_runtime::error::Error;
use gage_runtime::value::{json_to_value, value_to_json};
use gage_store::{NoteInput, NoteStore, NoteValue, StoreError};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Object, Protocol, Ref, Value, Vec as RuneVec, VmError};
use rune::{Any, ContextError, Module};

use crate::OUTPUT_SINK;
use crate::attachment::scan_attachments;
use crate::key::encode_key;
use crate::scan::{Scan, current, run, sql_str, string_column};
use crate::target::{Target, target_of};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("write_note", write_note).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<NoteWrite>()?;
    m.function_meta(NoteWrite::target)?;
    m.function_meta(NoteWrite::metadata)?;
    m.function_meta(NoteWrite::carry_forward)?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: NoteWrite| async move {
        do_write_note(w).await
    })?;
    m.ty::<Note>()?;
    m.function_meta(Note::debug)?;
    m.ty::<NotesQuery>()?;
    m.function_meta(notes)?;
    m.function_meta(NotesQuery::name)?;
    m.function_meta(NotesQuery::names)?;
    m.function_meta(crate::validate::notes_hwm)?;
    m.function_meta(crate::validate::notes_unseen)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NotesQuery| async move {
        fetch_notes(q.select).await
    })?;
    m.ty::<FilteredNotesQuery>()?;
    m.function_meta(FilteredNotesQuery::name)?;
    m.function_meta(FilteredNotesQuery::names)?;
    m.function_meta(crate::validate::filtered_notes_hwm)?;
    m.function_meta(crate::validate::filtered_notes_unseen)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: FilteredNotesQuery| async move {
        fetch_filtered_notes(q.select).await
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
    /// The target as given; resolved and validated at the await
    #[rune(skip)]
    target: Option<Target>,
    #[rune(skip)]
    metadata: Option<Value>,
    #[rune(skip)]
    carry_forward: Option<Value>,
}

fn write_note(name: &str, value: Value) -> NoteWrite {
    NoteWrite {
        name: name.to_string(),
        value,
        target: None,
        metadata: None,
        carry_forward: None,
    }
}

impl NoteWrite {
    /// What the note is about: a `Target`, or a `Session` or an
    /// `Attachment` as a whole-object target. The note links the
    /// target at the version the scan reads, so a carry can compare
    /// that version with the current one.
    #[rune::function(instance)]
    fn target(mut self, object: Value) -> Result<Self, VmError> {
        self.target = Some(target_of(&object)?);
        Ok(self)
    }

    /// The writer's payload, an object stored and returned verbatim.
    #[rune::function(instance)]
    fn metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Mark the note for carry-forward under `key`: a later scan's
    /// `carry_forward_notes(key)` links the note when its target
    /// session is in that scan. A string, or a tuple of strings and
    /// integers rendered colon-joined.
    #[rune::function(instance)]
    fn carry_forward(mut self, key: Value) -> Self {
        self.carry_forward = Some(key);
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
    /// The note's commit: the carried commit for a carried note,
    /// `None` for a note this scan wrote, which has none until apply
    #[rune(skip)]
    pub(crate) commit: Option<String>,
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
            VmError::panic("write_note is available only inside a active scan task")
        })?;
    let author = format!("task:{scanner}:{task}");

    // The target URL and the target's commit as the scan reads it,
    // which the note links in place of the tip
    let (target, pinned) = match &w.target {
        Some(t @ (Target::Session(id) | Target::SessionLines(id, _))) => {
            (Some(t.to_url()), ctx.member_commit(id).await?)
        }
        Some(t @ Target::Attachment(id)) => {
            let commit = scan_attachments()
                .await?
                .into_iter()
                .find(|a| a.id == *id)
                .map(|a| a.commit);
            (Some(t.to_url()), commit)
        }
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
    let key = match w.carry_forward.as_ref().map(encode_key).transpose() {
        Ok(k) => k,
        Err(e) => return Ok(Err(e)),
    };

    let id = new_uuid();
    let input = NoteInput {
        name: &w.name,
        value,
        author: &author,
        target: target.as_deref(),
        metadata: metadata.clone(),
        work_id: key.as_deref(),
    };
    let written = {
        let store = ctx.store.lock().await;
        NoteStore::from(&*store).write_to_dir(
            &ctx.paths.notes_dir().join(&id),
            &id,
            &input,
            &ctx.scan_id,
            pinned.as_deref(),
        )
    };
    match written {
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
        commit: None,
    }))
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
/// awaited. It has no argument, so its await is the list itself.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct NotesQuery {
    #[rune(skip)]
    pub(crate) select: NotesSelect,
}

/// `scan().notes()` narrowed by name. Its await is a `Result`
/// because a pattern is a syntax. The grammar today, `*` for any run
/// of characters and every other character literal, admits every
/// string, so the `Err` arm is the contract and not yet a case.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct FilteredNotesQuery {
    #[rune(skip)]
    pub(crate) select: NotesSelect,
}

/// What a notes read selects, shared by both query types.
#[derive(Clone)]
pub(crate) struct NotesSelect {
    /// Name patterns to keep, any of them; `None` keeps every note
    names: Option<Vec<String>>,
}

/// The notes this scan wrote or carried, read when awaited.
#[rune::function(instance)]
fn notes(_scan: Ref<Scan>) -> NotesQuery {
    NotesQuery {
        select: NotesSelect { names: None },
    }
}

impl NotesQuery {
    /// Keep the notes whose name matches `name`: an exact name, or a
    /// pattern in which `*` matches any run of characters.
    #[rune::function(instance)]
    fn name(self, name: &str) -> FilteredNotesQuery {
        FilteredNotesQuery {
            select: NotesSelect {
                names: Some(vec![name.to_string()]),
            },
        }
    }

    /// Keep the notes whose name matches any of `names`, each an exact
    /// name or a `*` pattern.
    #[rune::function(instance)]
    fn names(self, names: Ref<RuneVec>) -> Result<FilteredNotesQuery, VmError> {
        Ok(FilteredNotesQuery {
            select: NotesSelect {
                names: Some(patterns(&names)?),
            },
        })
    }
}

impl FilteredNotesQuery {
    /// As [`NotesQuery::name`], replacing the patterns so far.
    #[rune::function(instance)]
    fn name(mut self, name: &str) -> Self {
        self.select.names = Some(vec![name.to_string()]);
        self
    }

    /// As [`NotesQuery::names`], replacing the patterns so far.
    #[rune::function(instance)]
    fn names(mut self, names: Ref<RuneVec>) -> Result<Self, VmError> {
        self.select.names = Some(patterns(&names)?);
        Ok(self)
    }
}

/// The strings of a pattern list. A non-string element is a type
/// error.
pub(crate) fn patterns(names: &RuneVec) -> Result<Vec<String>, VmError> {
    let mut out = Vec::with_capacity(names.len());
    for v in names.iter() {
        let s = v
            .borrow_string_ref()
            .map_err(|e| VmError::panic(format!("names: expected strings: {e}")))?;
        out.push(s.to_string());
    }
    Ok(out)
}

/// The notes a filtered query selects; the `Err` arm is reserved for
/// a pattern the grammar rejects.
async fn fetch_filtered_notes(select: NotesSelect) -> Result<Result<Vec<Note>, Error>, VmError> {
    Ok(Ok(fetch_notes(select).await?))
}

/// Read the scan's notes through the scan-scoped `note` table,
/// filtered by name, oldest first and by id among equals.
pub(crate) async fn fetch_notes(select: NotesSelect) -> Result<Vec<Note>, VmError> {
    let ctx = current()?;
    let filter = match &select.names {
        Some(patterns) => format!(" WHERE {}", name_predicate("name", patterns)),
        None => String::new(),
    };
    let sql = format!(
        "SELECT id, name, target, author, value, text, metadata, created, commit \
         FROM note{filter} ORDER BY created, id"
    );
    let batches = run(ctx.scan_context().await?, &sql).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let names = string_column(batch, 1);
        let targets = string_column(batch, 2);
        let authors = string_column(batch, 3);
        let values = string_column(batch, 4);
        let texts = string_column(batch, 5);
        let metadatas = string_column(batch, 6);
        let createds = batch
            .column(7)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("note created is a timestamp column");
        let commits = string_column(batch, 8);
        let optional =
            |arr: &StringArray, i: usize| arr.is_valid(i).then(|| arr.value(i).to_string());
        for i in 0..batch.num_rows() {
            let id = ids.value(i);
            let value = match optional(texts, i) {
                Some(text) => rune::to_value(text).map_err(VmError::from)?,
                None => json_to_value(&parse_json(id, "value", values.value(i))?),
            };
            let metadata = match optional(metadatas, i) {
                Some(json) => json_to_value(&parse_json(id, "metadata", &json)?),
                None => rune::to_value(Object::new()).map_err(VmError::from)?,
            };
            out.push(Note {
                id: id.to_string(),
                name: names.value(i).to_string(),
                value,
                author: authors.value(i).to_string(),
                target: optional(targets, i),
                metadata,
                created: createds.value(i),
                commit: optional(commits, i),
            });
        }
    }
    Ok(out)
}

/// The `WHERE` clause matching `column` against name patterns: `*`
/// matches any run of characters, as in a task's `wants`; a pattern
/// without `*` is an exact name.
pub(crate) fn name_predicate(column: &str, patterns: &[String]) -> String {
    patterns
        .iter()
        .map(|p| {
            if p.contains('*') {
                let regex: String = p
                    .split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*");
                format!("regexp_like({column}, '^{}$')", sql_str(&regex))
            } else {
                format!("{column} = '{}'", sql_str(p))
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn parse_json(id: &str, column: &str, text: &str) -> Result<serde_json::Value, VmError> {
    serde_json::from_str(text)
        .map_err(|e| VmError::panic(format!("note {id} {column} is not JSON: {e}")))
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

    /// `names` on a filtered query borrows its list too, so both the
    /// first and the replacing list stay readable.
    #[test]
    fn filtered_notes_names_leaves_the_caller_list_readable() {
        let mut vm = vm(r#"
            pub fn check(scan) {
                let first = ["a"];
                let second = ["b", "c"];
                let query = scan.notes().names(first).names(second);
                (first[0], second.len())
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
        let (first, len): (String, i64) = rune::from_value(output).unwrap();
        assert_eq!((first.as_str(), len), ("a", 2));
    }

    /// `target` borrows its argument, so the caller's attachment is
    /// still readable afterwards.
    #[test]
    fn target_leaves_the_caller_value_readable() {
        let mut vm = vm(r#"
            use gage::write_note;

            pub fn check(a) {
                let w = write_note("n", "v").target(a);
                a.id
            }
            "#);
        let attachment = crate::attachment::Attachment {
            id: "att-1".into(),
            name: None,
            key: None,
            targets: rune::to_value(Vec::<String>::new()).unwrap(),
            root: "/r".into(),
            digest: None,
            commit: "c".into(),
        };
        let output = vm.call(["check"], (attachment,)).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let id: String = rune::from_value(output).unwrap();
        assert_eq!(id, "att-1");
    }
}
