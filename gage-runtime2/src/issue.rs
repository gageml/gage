//! `write_issue(name, title, description)`: an issue written to the
//! scan's staging, and `issues()`: the store's issues read back.
//!
//! The builder carries the name, title, description, the cited note
//! ids, and the initial status. Awaiting it checks each cited note
//! against the scan's staged notes and the store, writes the issue
//! tree under the scan's staging (`gage_store::IssueStore::stage`),
//! and returns the [`Issue`]. The runtime sets `author` to
//! `task:<scanner>:<task>` and `attrs.scan` to the running scan.
//! Apply creates the object, resolving the cited notes to their
//! commits, and links it from the scan through `issues.link`. An
//! issue has no target; a session it concerns is reached through the
//! notes it cites. Bad input is `Error::Args`; a failure to reach
//! staging or the store is a VM error.
//!
//! `issues()` is an [`IssuesQuery`]; awaiting it reads every live
//! issue in the store plus the issues this scan has staged, since a
//! task sees what its upstream tasks wrote. Issues are store-wide, so
//! the query hangs off no scan value. `.name(..)` and `.status(..)`
//! each take one value or a list, and a list matches any of its
//! members.

use std::fs;
use std::io;

use gage_core::uuid::new_uuid;
use gage_runtime::error::Error;
use gage_store::{IssueInput, IssueStatus, IssueStore, NOTE_TYPE, StoreError};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Value, Vec as RuneVec, VmError};
use rune::{Any, ContextError, Module};

use crate::OUTPUT_SINK;
use crate::note::Note;
use crate::scan::{ScanContext, current};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("write_issue", write_issue).build()?;
    m.function("issues", issues).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<IssueWrite>()?;
    m.function_meta(IssueWrite::evidence)?;
    m.function_meta(IssueWrite::pending)?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: IssueWrite| async move {
        do_write_issue(w).await
    })?;
    m.ty::<Issue>()?;
    m.function_meta(Issue::debug)?;
    m.ty::<IssuesQuery>()?;
    m.function_meta(IssuesQuery::name)?;
    m.function_meta(IssuesQuery::status)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: IssuesQuery| async move {
        fetch_issues(q).await
    })?;
    Ok(m)
}

/// The builder `write_issue(name, title, description)` returns.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct IssueWrite {
    #[rune(skip)]
    name: String,
    #[rune(skip)]
    title: String,
    #[rune(skip)]
    description: String,
    /// Cited note ids in citation order, as given
    #[rune(skip)]
    evidence: Vec<String>,
    /// The first evidence argument that was not a note id or a list
    /// of them, reported at the await
    #[rune(skip)]
    evidence_error: Option<Error>,
    #[rune(skip)]
    status: IssueStatus,
}

fn write_issue(name: &str, title: &str, description: &str) -> IssueWrite {
    IssueWrite {
        name: name.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        evidence: Vec::new(),
        evidence_error: None,
        status: IssueStatus::Open,
    }
}

impl IssueWrite {
    /// Cite notes as evidence: a note id, a `Note`, or a list of
    /// either. Repeated calls accumulate.
    #[rune::function(instance)]
    fn evidence(mut self, notes: Value) -> Self {
        match note_ids(&notes) {
            Ok(ids) => self.evidence.extend(ids),
            Err(e) => {
                if self.evidence_error.is_none() {
                    self.evidence_error = Some(e);
                }
            }
        }
        self
    }

    /// Write the issue with status `pending`, for reconciliation by
    /// the resolve workflow, instead of `open`.
    #[rune::function(instance)]
    fn pending(mut self) -> Self {
        self.status = IssueStatus::Pending;
        self
    }
}

/// The note ids in an evidence argument, read without taking the
/// caller's value.
fn note_ids(v: &Value) -> Result<Vec<String>, Error> {
    if let Ok(list) = v.borrow_ref::<RuneVec>() {
        let mut ids = Vec::with_capacity(list.len());
        for item in list.iter() {
            ids.push(note_id(item)?);
        }
        return Ok(ids);
    }
    Ok(vec![note_id(v)?])
}

fn note_id(v: &Value) -> Result<String, Error> {
    if let Ok(s) = v.borrow_string_ref() {
        if s.trim().is_empty() {
            return Err(Error::Args("evidence note id is empty".into()));
        }
        return Ok(s.trim().to_string());
    }
    if let Ok(note) = v.borrow_ref::<Note>() {
        return Ok(note.id.clone());
    }
    Err(Error::Args(
        "evidence must be a note id, a Note, or a list of either".into(),
    ))
}

/// An issue, as returned to the scanner.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Issue {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: String,
    #[rune(get)]
    pub title: String,
    #[rune(get)]
    pub description: Option<String>,
    #[rune(get)]
    pub author: String,
    /// `pending`, `open`, or `closed`
    #[rune(get)]
    pub status: String,
    /// `completed`, `wontfix`, or `duplicate` when closed
    #[rune(get)]
    pub status_reason: Option<String>,
    /// The scan the issue was written during, or `None`
    #[rune(get)]
    pub scan: Option<String>,
    /// The ids of the cited notes, a list of strings
    #[rune(get)]
    pub evidence: Value,
    /// UNIX time millis
    #[rune(get)]
    pub created: i64,
}

impl Issue {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Issue {{ id: {:?}, name: {:?}, title: {:?}, status: {:?}, evidence: ",
            self.id, self.name, self.title, self.status
        )?;
        self.evidence.debug_fmt(f)?;
        write!(f, " }}")?;
        Ok(())
    }
}

/// The outer error is a VM error; the inner is the scanner's `Result`.
type Written = Result<Result<Issue, Error>, VmError>;

async fn do_write_issue(w: IssueWrite) -> Written {
    let ctx = current()?;
    let (scanner, task) = OUTPUT_SINK
        .try_with(|sink| (sink.scanner.clone(), sink.task.clone()))
        .map_err(|_outside_task| {
            VmError::panic("write_issue is available only inside a running scan task")
        })?;
    if let Some(e) = w.evidence_error {
        return Ok(Err(e));
    }
    let author = format!("task:{scanner}:{task}");
    if let Err(e) = check_evidence(&ctx, &w.evidence).await? {
        return Ok(Err(e));
    }
    let id = new_uuid();
    let description = (!w.description.trim().is_empty()).then_some(w.description.as_str());
    let input = IssueInput {
        name: &w.name,
        title: &w.title,
        description,
        author: &author,
        status: w.status,
        evidence: &w.evidence,
    };
    let staged = {
        let store = ctx.store.lock().await;
        IssueStore::from(&*store).stage(&ctx.paths.issues_dir.join(&id), &id, &input, &ctx.scan_id)
    };
    match staged {
        Ok(()) => {}
        Err(StoreError::IssueInput(m)) => return Ok(Err(Error::Args(format!("write_issue: {m}")))),
        Err(e) => return Err(VmError::panic(format!("write_issue: {e}"))),
    }
    tracing::debug!(id, name = w.name, author, "write_issue");

    let mut evidence = Vec::with_capacity(w.evidence.len());
    for note in &w.evidence {
        if !evidence.contains(note) {
            evidence.push(note.clone());
        }
    }
    Ok(Ok(Issue {
        id,
        name: w.name,
        title: w.title,
        description: description.map(String::from),
        author,
        status: w.status.as_str().to_string(),
        status_reason: None,
        scan: Some(ctx.scan_id.clone()),
        evidence: rune::to_value(evidence).map_err(VmError::from)?,
        created: gage_core::datetime::now_ms(),
    }))
}

/// Every cited note must be one this scan staged or a live note in
/// the store. A cited id that is neither is the scanner's error.
async fn check_evidence(ctx: &ScanContext, ids: &[String]) -> Result<Result<(), Error>, VmError> {
    for id in ids {
        if ctx.paths.notes_dir.join(id).is_dir() {
            continue;
        }
        let store = ctx.store.lock().await;
        match store.resolve_in(id, Some(NOTE_TYPE)) {
            Ok(found) if !found.deleted && found.id == *id => {}
            Ok(found) if found.deleted => {
                return Ok(Err(Error::Args(format!(
                    "write_issue evidence: note {id} is deleted"
                ))));
            }
            Ok(_) => {
                return Ok(Err(Error::Args(format!(
                    "write_issue evidence: {id} is not a full note id"
                ))));
            }
            Err(
                e @ (StoreError::ObjectNotFound(_)
                | StoreError::AmbiguousId(..)
                | StoreError::WrongType { .. }),
            ) => {
                return Ok(Err(Error::Args(format!("write_issue evidence: {e}"))));
            }
            Err(e) => return Err(VmError::panic(format!("write_issue evidence: {e}"))),
        }
    }
    Ok(Ok(()))
}

/// The value of `issues()`: the store's issues plus this scan's
/// staged issues, read when awaited.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct IssuesQuery {
    /// Names to keep; `None` keeps every issue
    #[rune(skip)]
    names: Option<Vec<String>>,
    /// Statuses to keep; `None` keeps every issue
    #[rune(skip)]
    statuses: Option<Vec<IssueStatus>>,
    /// The first filter argument that was malformed, reported at the
    /// await
    #[rune(skip)]
    error: Option<Error>,
}

fn issues() -> IssuesQuery {
    IssuesQuery {
        names: None,
        statuses: None,
        error: None,
    }
}

impl IssuesQuery {
    /// Keep the issues with this name, or with any name in a list.
    #[rune::function(instance)]
    fn name(mut self, names: Value) -> Self {
        match strings(&names, "name") {
            Ok(list) => self.names = Some(list),
            Err(e) => self.fail(e),
        }
        self
    }

    /// Keep the issues with this status, or with any status in a
    /// list: `"pending"`, `"open"`, or `"closed"`.
    #[rune::function(instance)]
    fn status(mut self, statuses: Value) -> Self {
        match strings(&statuses, "status") {
            Ok(list) => {
                let mut parsed = Vec::with_capacity(list.len());
                for s in &list {
                    match s.parse::<IssueStatus>() {
                        Ok(status) => parsed.push(status),
                        Err(e) => {
                            self.fail(Error::Args(format!("issues status: {e}")));
                            return self;
                        }
                    }
                }
                self.statuses = Some(parsed);
            }
            Err(e) => self.fail(e),
        }
        self
    }

    fn fail(&mut self, e: Error) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }
}

/// A string or a list of strings, read without taking the caller's
/// value.
fn strings(v: &Value, what: &str) -> Result<Vec<String>, Error> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(vec![s.to_string()]);
    }
    if let Ok(list) = v.borrow_ref::<RuneVec>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            let s = item.borrow_string_ref().map_err(|_not_a_string| {
                Error::Args(format!(
                    "issues {what}: expected a string or a list of strings"
                ))
            })?;
            out.push(s.to_string());
        }
        return Ok(out);
    }
    Err(Error::Args(format!(
        "issues {what}: expected a string or a list of strings"
    )))
}

/// Read the store's live issues and this scan's staged issues,
/// filtered, oldest first and by id among equals.
async fn fetch_issues(q: IssuesQuery) -> Result<Result<Vec<Issue>, Error>, VmError> {
    if let Some(e) = q.error {
        return Ok(Err(e));
    }
    let ctx = current()?;
    let mut out = stored_issues(&ctx).await?;
    out.extend(staged_issues(&ctx).await?);
    out.retain(|i| {
        q.names.as_ref().is_none_or(|names| names.contains(&i.name))
            && q.statuses
                .as_ref()
                .is_none_or(|statuses| statuses.iter().any(|s| s.as_str() == i.status))
    });
    out.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    Ok(Ok(out))
}

/// Every live issue in the store, with its evidence as note ids.
async fn stored_issues(ctx: &ScanContext) -> Result<Vec<Issue>, VmError> {
    let store = ctx.store.lock().await;
    let issues = IssueStore::from(&*store);
    let records = issues
        .iter()
        .map_err(|e| VmError::panic(format!("issues: {e}")))?;
    let mut out = Vec::new();
    for record in records {
        let full = record.map_err(|e| VmError::panic(format!("issues: {e}")))?;
        let mut evidence = Vec::with_capacity(full.evidence.len());
        for sha in &full.evidence {
            let note = store
                .read_header(sha)
                .map_err(|e| VmError::panic(format!("issue {} evidence {sha}: {e}", full.id)))?;
            evidence.push(note.id);
        }
        out.push(Issue {
            id: full.id,
            name: full.name,
            title: full.title,
            description: full.description,
            author: full.author,
            status: full.status.as_str().to_string(),
            status_reason: full.status_reason.map(|r| r.as_str().to_string()),
            scan: full.scan,
            evidence: rune::to_value(evidence).map_err(VmError::from)?,
            created: full.created_ms,
        });
    }
    Ok(out)
}

/// The issues staged under the scan's issues directory, in id order.
async fn staged_issues(ctx: &ScanContext) -> Result<Vec<Issue>, VmError> {
    let entries = match fs::read_dir(&ctx.paths.issues_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(VmError::panic(format!("staged issues: {e}"))),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| VmError::panic(format!("staged issues: {e}")))?
            .path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    let store = ctx.store.lock().await;
    let issues = IssueStore::from(&*store);
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let staged = issues
            .read_staged(&dir)
            .map_err(|e| VmError::panic(format!("staged issue {}: {e}", dir.display())))?;
        out.push(Issue {
            id: staged.id,
            name: staged.name,
            title: staged.title,
            description: staged.description,
            author: staged.author,
            status: staged.status.as_str().to_string(),
            status_reason: None,
            scan: staged.scan,
            evidence: rune::to_value(staged.evidence).map_err(VmError::from)?,
            created: staged.created_ms,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rune::Vm;
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources};

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

    /// `evidence`, `name`, and `status` borrow their arguments, so the
    /// caller's values are still readable afterwards.
    #[test]
    fn builders_leave_the_caller_values_readable() {
        let mut vm = vm(r#"
            pub fn check() {
                let notes = ["a", "b"];
                let one = "c";
                let write = gage::write_issue("n", "t", "d").evidence(notes).evidence(one);
                let names = ["x", "y"];
                let statuses = ["open", "pending"];
                let query = gage::issues().name(names).status(statuses).status("closed");
                (notes.len(), notes[1], one, names[0], statuses.len())
            }
            "#);
        let output = vm.call(["check"], ()).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (len, second, one, name, count): (i64, String, String, String, i64) =
            rune::from_value(output).unwrap();
        assert_eq!(
            (len, second.as_str(), one.as_str(), name.as_str(), count),
            (2, "b", "c", "x", 2)
        );
    }
}
