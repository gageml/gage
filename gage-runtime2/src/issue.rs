//! `write_issue(name, title, description)`: an issue written to the
//! scan directory, and `scan().issues()`: the scan's issues read back.
//!
//! The builder carries the name, title, description, the cited note
//! ids, the initial status, and the optional replace key. Awaiting it
//! checks each cited note against the scan's own notes and the
//! store, writes the issue tree into the scan directory
//! (`gage_store::IssueStore::write_to_dir`), and returns the [`Issue`]. The
//! runtime sets `author` to `task:<scanner>:<task>` and `attrs.scan`
//! to the active scan. Apply creates the object, resolving the cited
//! notes to their commits, and links it from the scan through
//! `issues.link`. An issue has no target; a session it concerns is
//! reached through the notes it cites. Bad input is `Error::Args`; a
//! failure to reach the scan directory or the store is a VM error.
//!
//! `.key(key)` gives the issue an identity across scans, a string or
//! a tuple encoded like a watermark key. When a live issue in the
//! store carries the same key, the write is recorded as a replacement
//! of it: apply writes the new state as that issue's next commit, and
//! the scan links that commit. A second write under the same key in
//! one scan replaces the first. `.once()` changes what the key does:
//! when an issue under the key exists, written by this scan or live in
//! the store in any status, nothing is written and the existing issue
//! is returned. A closed issue counts, so a condition reported once
//! and closed is not reported again. `.once()` without `.key()` is an
//! `Args` error at the await.
//!
//! `scan().issues()` is an [`IssuesQuery`]; awaiting it reads the
//! issues this scan has written through the scan-scoped `issue`
//! table, so a task sees what its upstream tasks wrote. `.name(..)`
//! and `.status(..)` each take one value or a list, and a list matches
//! any of its members.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use datafusion::arrow::array::{Array, StringArray, TimestampMillisecondArray};
use gage_core::uuid::new_uuid;
use gage_runtime::error::Error;
use gage_store::{
    IssueDirRecord, IssueFull, IssueInput, IssueStatus, IssueStore, NOTE_TYPE, Store, StoreError,
};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Value, Vec as RuneVec, VmError};
use rune::{Any, ContextError, Module};

use crate::OUTPUT_SINK;
use crate::key::encode_key;
use crate::note::Note;
use crate::scan::{Scan, ScanContext, current, run, sql_str, string_column};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("write_issue", write_issue).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<IssueWrite>()?;
    m.function_meta(IssueWrite::evidence)?;
    m.function_meta(IssueWrite::pending)?;
    m.function_meta(IssueWrite::key)?;
    m.function_meta(IssueWrite::once)?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: IssueWrite| async move {
        do_write_issue(w).await
    })?;
    m.ty::<Issue>()?;
    m.function_meta(Issue::debug)?;
    m.ty::<IssuesQuery>()?;
    m.function_meta(Scan::issues)?;
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
    /// The identity key as given; encoded at the await
    #[rune(skip)]
    key: Option<Value>,
    /// What a write does when an issue under the key exists
    #[rune(skip)]
    policy: KeyPolicy,
}

/// The action when the key names an existing issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyPolicy {
    /// Record this write as the existing issue's next state
    Replace,
    /// Write nothing and return the existing issue
    Once,
}

fn write_issue(name: &str, title: &str, description: &str) -> IssueWrite {
    IssueWrite {
        name: name.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        evidence: Vec::new(),
        evidence_error: None,
        status: IssueStatus::Open,
        key: None,
        policy: KeyPolicy::Replace,
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

    /// Give the issue an identity across scans. A later write under
    /// the same key replaces the live issue instead of writing a
    /// second one. `key` is a string, or a tuple of strings and
    /// integers rendered colon-joined, and is stored on the issue as
    /// `key`.
    #[rune::function(instance)]
    fn key(mut self, key: Value) -> Self {
        self.key = Some(key);
        self
    }

    /// Write only when no issue under the key exists, in any status,
    /// and return the existing issue otherwise. Requires `.key()`.
    #[rune::function(instance)]
    fn once(mut self) -> Self {
        self.policy = KeyPolicy::Once;
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
    #[rune(get)]
    pub key: Option<String>,
    /// The ids of the cited notes, a list of strings
    #[rune(get)]
    pub evidence: Value,
    /// UNIX time millis
    #[rune(get)]
    pub created: i64,
}

impl Issue {
    /// A live issue read from the store, its evidence resolved to
    /// note ids.
    fn from_full(store: &Store, full: IssueFull) -> Result<Issue, VmError> {
        let mut evidence = Vec::with_capacity(full.evidence.len());
        for sha in &full.evidence {
            let note = store
                .read_header(sha)
                .map_err(|e| VmError::panic(format!("issue {} evidence {sha}: {e}", full.id)))?;
            evidence.push(note.id);
        }
        Ok(Issue {
            id: full.id,
            name: full.name,
            title: full.title,
            description: full.description,
            author: full.author,
            status: full.status.as_str().to_string(),
            status_reason: full.status_reason.map(|r| r.as_str().to_string()),
            scan: full.scan,
            key: full.key,
            evidence: rune::to_value(evidence).map_err(VmError::from)?,
            created: full.created_ms,
        })
    }

    /// An issue this scan wrote, read from the scan directory.
    fn from_dir_record(record: IssueDirRecord) -> Result<Issue, VmError> {
        Ok(Issue {
            id: record.id,
            name: record.name,
            title: record.title,
            description: record.description,
            author: record.author,
            status: record.status.as_str().to_string(),
            status_reason: None,
            scan: record.scan,
            key: record.key,
            evidence: rune::to_value(record.evidence).map_err(VmError::from)?,
            created: record.created_ms,
        })
    }

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
pub(crate) type Written = Result<Result<Issue, Error>, VmError>;

/// Write an issue on behalf of the `IssueWrite` Gage tool: the call's
/// title, description, and cited notes under the name and status the
/// scanner configured. The write is the one `write_issue(..).await`
/// performs, with no replace key.
pub(crate) async fn write_issue_tool(
    name: String,
    title: String,
    description: Option<String>,
    evidence: Vec<String>,
    status: IssueStatus,
) -> Written {
    write_issue_spec(IssueSpec {
        name,
        title,
        description: description.unwrap_or_default(),
        evidence,
        status,
        key: None,
        policy: KeyPolicy::Replace,
    })
    .await
}

/// A write with its Rune-held inputs resolved: the builder's fields
/// as owned strings, so the write is a `Send` future the MCP service
/// can run.
struct IssueSpec {
    name: String,
    title: String,
    description: String,
    evidence: Vec<String>,
    status: IssueStatus,
    key: Option<String>,
    policy: KeyPolicy,
}

async fn do_write_issue(w: IssueWrite) -> Written {
    if let Some(e) = w.evidence_error {
        return Ok(Err(e));
    }
    if w.policy == KeyPolicy::Once && w.key.is_none() {
        return Ok(Err(Error::Args(
            "write_issue: once() requires key()".into(),
        )));
    }
    let key = match w.key.as_ref().map(encode_key).transpose() {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    write_issue_spec(IssueSpec {
        name: w.name,
        title: w.title,
        description: w.description,
        evidence: w.evidence,
        status: w.status,
        key,
        policy: w.policy,
    })
    .await
}

async fn write_issue_spec(w: IssueSpec) -> Written {
    let ctx = current()?;
    let (scanner, task) = OUTPUT_SINK
        .try_with(|sink| (sink.scanner.clone(), sink.task.clone()))
        .map_err(|_outside_task| {
            VmError::panic("write_issue is available only inside a active scan task")
        })?;
    let author = format!("task:{scanner}:{task}");
    if let Err(e) = check_evidence(&ctx, &w.evidence).await? {
        return Ok(Err(e));
    }
    let key = w.key;
    let description = (!w.description.trim().is_empty()).then_some(w.description.as_str());
    let input = IssueInput {
        name: &w.name,
        title: &w.title,
        description,
        author: &author,
        status: w.status,
        evidence: &w.evidence,
        key: key.as_deref(),
    };
    let store = ctx.store.lock().await;
    let issues = IssueStore::from(&*store);
    // A key names the issue to replace or return: the one this scan
    // already wrote under the key, else the live one in the store
    let prev = match &key {
        Some(key) => prior_issue(&ctx, &issues, key)?,
        None => None,
    };
    if w.policy == KeyPolicy::Once {
        match prev {
            Some(Prior::InDir(id)) => {
                let dir = ctx.paths.issues_dir().join(&id);
                let record = issues
                    .read_from_dir(&dir)
                    .map_err(|e| VmError::panic(format!("issue dir {id}: {e}")))?;
                tracing::debug!(id, name = w.name, key, "write_issue: kept the scan's own");
                return Ok(Ok(Issue::from_dir_record(record)?));
            }
            Some(Prior::Stored(live)) => {
                tracing::debug!(id = live.id, name = w.name, key, "write_issue: kept");
                return Ok(Ok(Issue::from_full(&store, *live)?));
            }
            None => {}
        }
    }
    let (id, written) = match prev {
        Some(Prior::InDir(id)) => {
            let dir = ctx.paths.issues_dir().join(&id);
            fs::remove_dir_all(&dir)
                .map_err(|e| VmError::panic(format!("write_issue: remove {id}: {e}")))?;
            // The first write replaced a live issue, or created one
            match issues.get(&id) {
                Ok(live) => (
                    id.clone(),
                    issues.write_replace_to_dir(&dir, &input, &ctx.scan_id, &live),
                ),
                Err(StoreError::ObjectNotFound(_)) => (
                    id.clone(),
                    issues.write_to_dir(&dir, &id, &input, &ctx.scan_id),
                ),
                Err(e) => return Err(VmError::panic(format!("write_issue: read issue {id}: {e}"))),
            }
        }
        Some(Prior::Stored(live)) => {
            let id = live.id.clone();
            let dir = ctx.paths.issues_dir().join(&id);
            (
                id,
                issues.write_replace_to_dir(&dir, &input, &ctx.scan_id, &live),
            )
        }
        None => {
            let id = new_uuid();
            let dir = ctx.paths.issues_dir().join(&id);
            (
                id.clone(),
                issues.write_to_dir(&dir, &id, &input, &ctx.scan_id),
            )
        }
    };
    drop(store);
    match written {
        Ok(()) => {}
        Err(StoreError::IssueInput(m)) => return Ok(Err(Error::Args(format!("write_issue: {m}")))),
        Err(e) => return Err(VmError::panic(format!("write_issue: {e}"))),
    }
    tracing::debug!(id, name = w.name, author, key, "write_issue");

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
        key,
        evidence: rune::to_value(evidence).map_err(VmError::from)?,
        created: gage_core::datetime::now_ms(),
    }))
}

/// The issue a replace key names.
enum Prior {
    /// Written by this scan, in the scan directory, by id
    InDir(String),
    /// Live in the store
    Stored(Box<IssueFull>),
}

/// The issue `key` names: one this scan wrote under the key, else
/// the live issue in the store carrying it. Two live issues under one
/// key is a store fault.
fn prior_issue(
    ctx: &ScanContext,
    issues: &IssueStore<'_>,
    key: &str,
) -> Result<Option<Prior>, VmError> {
    for dir in issue_dirs(ctx)? {
        let record = issues
            .read_from_dir(&dir)
            .map_err(|e| VmError::panic(format!("issue dir {}: {e}", dir.display())))?;
        if record.key.as_deref() == Some(key) {
            return Ok(Some(Prior::InDir(record.id)));
        }
    }
    let tips = issues
        .query()
        .key(key)
        .tips()
        .map_err(|e| VmError::panic(format!("write_issue: replace key {key}: {e}")))?;
    match tips.as_slice() {
        [] => Ok(None),
        [tip] => {
            let live = issues
                .at_commit(&tip.sha)
                .map_err(|e| VmError::panic(format!("write_issue: read issue {}: {e}", tip.id)))?;
            Ok(Some(Prior::Stored(Box::new(live))))
        }
        many => Err(VmError::panic(format!(
            "write_issue: replace key {key} names {} live issues",
            many.len()
        ))),
    }
}

/// Every cited note must be one this scan wrote or a live note in
/// the store. A cited id that is neither is the scanner's error.
async fn check_evidence(ctx: &ScanContext, ids: &[String]) -> Result<Result<(), Error>, VmError> {
    for id in ids {
        if ctx.paths.notes_dir().join(id).is_dir() {
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

/// The value of `scan().issues()`: the issues this scan wrote, read
/// when awaited.
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

impl Scan {
    /// The issues this scan wrote, read when awaited.
    #[rune::function(instance)]
    fn issues(&self) -> IssuesQuery {
        IssuesQuery {
            names: None,
            statuses: None,
            error: None,
        }
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

/// Read the scan's issues through the scan-scoped `issue` table,
/// filtered, oldest first and by id among equals, each with its
/// evidence from `issue_evidence`.
async fn fetch_issues(q: IssuesQuery) -> Result<Result<Vec<Issue>, Error>, VmError> {
    if let Some(e) = q.error {
        return Ok(Err(e));
    }
    let ctx = current()?;
    let mut predicates = Vec::new();
    if let Some(names) = &q.names {
        predicates.push(format!(
            "name IN ({})",
            quoted_list(names.iter().map(String::as_str))
        ));
    }
    if let Some(statuses) = &q.statuses {
        predicates.push(format!(
            "status IN ({})",
            quoted_list(statuses.iter().map(|s| s.as_str()))
        ));
    }
    let filter = if predicates.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", predicates.join(" AND "))
    };
    let sql = format!(
        "SELECT id, name, title, description, author, status, status_reason, scan, key, \
                created \
         FROM issue{filter} ORDER BY created, id"
    );
    let df_ctx = ctx.scan_context().await?;
    let batches = run(df_ctx, &sql).await?;
    let evidence = run(df_ctx, "SELECT issue_id, note_id FROM issue_evidence").await?;
    let mut cited: HashMap<String, Vec<String>> = HashMap::new();
    for batch in &evidence {
        let issues = string_column(batch, 0);
        let notes = string_column(batch, 1);
        for i in 0..batch.num_rows() {
            cited
                .entry(issues.value(i).to_string())
                .or_default()
                .push(notes.value(i).to_string());
        }
    }
    let mut out = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let names = string_column(batch, 1);
        let titles = string_column(batch, 2);
        let descriptions = string_column(batch, 3);
        let authors = string_column(batch, 4);
        let statuses = string_column(batch, 5);
        let reasons = string_column(batch, 6);
        let scans = string_column(batch, 7);
        let keys = string_column(batch, 8);
        let createds = batch
            .column(9)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("issue created is a timestamp column");
        let optional =
            |arr: &StringArray, i: usize| arr.is_valid(i).then(|| arr.value(i).to_string());
        for i in 0..batch.num_rows() {
            let id = ids.value(i).to_string();
            let evidence = cited.remove(&id).unwrap_or_default();
            out.push(Issue {
                name: names.value(i).to_string(),
                title: titles.value(i).to_string(),
                description: optional(descriptions, i),
                author: authors.value(i).to_string(),
                status: statuses.value(i).to_string(),
                status_reason: optional(reasons, i),
                scan: optional(scans, i),
                key: optional(keys, i),
                evidence: rune::to_value(evidence).map_err(VmError::from)?,
                created: createds.value(i),
                id,
            });
        }
    }
    Ok(Ok(out))
}

/// `'a', 'b', ...` for an `IN` list.
fn quoted_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items
        .map(|s| format!("'{}'", sql_str(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The scan directory's issue directories, in id order.
fn issue_dirs(ctx: &ScanContext) -> Result<Vec<PathBuf>, VmError> {
    let entries = match fs::read_dir(ctx.paths.issues_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(VmError::panic(format!("scan directory issues: {e}"))),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| VmError::panic(format!("scan directory issues: {e}")))?
            .path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
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
            pub fn check(scan) {
                let notes = ["a", "b"];
                let one = "c";
                let write = gage::write_issue("n", "t", "d").evidence(notes).evidence(one);
                let names = ["x", "y"];
                let statuses = ["open", "pending"];
                let query = scan.issues().name(names).status(statuses).status("closed");
                (notes.len(), notes[1], one, names[0], statuses.len())
            }
            "#);
        let scan = crate::scan::Scan {
            id: "scan".into(),
            dataset: None,
        };
        let output = vm.call(["check"], (scan,)).unwrap();
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
