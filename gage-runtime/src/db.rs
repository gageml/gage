use std::collections::HashSet;
use std::sync::OnceLock;

use gage_db::issue::{
    self, Issue as DbIssue, IssueEvidence as DbIssueEvidence, IssueFilters, IssueStatus,
    IssueStatusFilter, StatusReason,
};
use gage_db::note::{self, Note as DbNote, NoteFilters, NoteValue};
use gage_db::rusqlite::Connection;
use gage_db::scan::{ScanLinkRole, insert_scan_issue, insert_scan_note};
use gage_db::target::{NoteTarget, ProjectTarget, ScanTarget, SessionTarget};
use rune::Any;
use rune::alloc;
use rune::alloc::clone::TryClone;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Object, Protocol, Ref, Value, Vec as RuneVec, VmError};
use rune::{ContextError, Module};
use tracing::warn;

use crate::config::Project;
use crate::error::Error;
use crate::scan::{Scan, Session};
use crate::state::current_scan_ctx;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("write_note", |n: &Object| -> alloc::Result<NoteInsert> {
        Ok(NoteInsert::new(n.try_clone()?))
    })
    .build()?;
    m.function_meta(NoteInsert::replace_prev)?;
    m.function_meta(NoteInsert::keep_prev)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NoteInsert| async move {
        do_write_note(q)
    })?;

    m.function("write_issue", |t: &Object| -> alloc::Result<IssueInsert> {
        Ok(IssueInsert::new(t.try_clone()?))
    })
    .build()?;
    m.function_meta(IssueInsert::keep_status)?;
    m.function_meta(IssueInsert::open_on_new_evidence)?;
    m.function_meta(IssueInsert::open_on_changed_evidence)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: IssueInsert| async move {
        do_write_issue(q)
    })?;

    m.function("global", || Global).build()?;
    m.function_meta(Global::issues)?;
    m.function_meta(IssuesQuery::status)?;
    m.function_meta(IssuesQuery::name)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: IssuesQuery| async move {
        fetch_issues(q)
    })?;

    m.function(
        "update_issue",
        |id: Ref<str>, args: Ref<Object>| async move { do_update_issue(&id, &args) },
    )
    .build()?;

    m.function_meta(session_notes)?;
    m.function_meta(scan_notes)?;
    m.function_meta(project_notes)?;
    m.function_meta(NotesQuery::name)?;
    m.function_meta(NotesQuery::names)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NotesQuery| async move {
        fetch_notes(q)
    })?;

    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Note>()?;
    m.field_function(&Protocol::GET, "metadata", Note::get_metadata)?;
    m.field_function(&Protocol::GET, "target", Note::get_target)?;
    m.function_meta(Note::debug)?;
    m.ty::<NoteInsert>()?;
    m.ty::<RuneNoteTarget>()?;
    m.ty::<NotesQuery>()?;
    m.ty::<Issue>()?;
    m.function_meta(Issue::debug)?;
    m.ty::<IssueInsert>()?;
    m.ty::<Global>()?;
    m.ty::<IssuesQuery>()?;
    Ok(())
}

/// Unscoped query surface over everything Gage has recorded — the
/// counterpart to `scan()`, whose builders are bound to the current
/// scan. Returned by `global()`.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Global;

impl Global {
    /// Query issues: `global().issues().status("pending").await?`
    #[rune::function(instance)]
    fn issues(&self) -> IssuesQuery {
        IssuesQuery::new()
    }
}

/// Builder returned by `Global::issues`. Queries the full issue table —
/// issues are global, not scan-scoped. Defaults to open issues;
/// `.status(..)` selects `"pending"`, `"open"`, `"closed"`, or `"any"`.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct IssuesQuery {
    #[rune(skip)]
    status: Option<String>,
    #[rune(skip)]
    name: Option<String>,
}

impl IssuesQuery {
    fn new() -> Self {
        Self {
            status: None,
            name: None,
        }
    }

    #[rune::function(instance)]
    fn status(mut self, status: Ref<str>) -> Self {
        self.status = Some(status.to_owned());
        self
    }

    #[rune::function(instance)]
    fn name(mut self, name: Ref<str>) -> Self {
        self.name = Some(name.to_owned());
        self
    }
}

fn fetch_issues(q: IssuesQuery) -> super::Result<Vec<Issue>> {
    let ctx = current_scan_ctx();
    let status = match q.status.as_deref() {
        None | Some("open") => IssueStatusFilter::Open,
        Some("pending") => IssueStatusFilter::Pending,
        Some("closed") => IssueStatusFilter::Closed,
        Some("any") => IssueStatusFilter::Any,
        Some(other) => {
            return Err(Error::Args(format!(
                "unknown issue status filter '{other}'"
            )));
        }
    };
    let filters = IssueFilters {
        status,
        name: q.name,
        ..Default::default()
    };
    let db = ctx.db.lock().unwrap();
    let db_issues = issue::find(&db, &filters).map_err(|e| Error::Db(e.to_string()))?;
    Ok(db_issues.into_iter().map(Issue::from).collect())
}

/// Update an issue: `update_issue(id, #{ status, status_reason?, message? })`.
/// `status` is `"open"`, `"closed"`, or `"pending"`. `status_reason`
/// (`"completed"`, `"skipped"`, or `"duplicate"`) applies only when
/// closing; a missing reason defaults to `"completed"`.
fn do_update_issue(id: &str, args: &Object) -> super::Result<()> {
    let ctx = current_scan_ctx();
    let status: IssueStatus = required_string(args, "status")?
        .parse()
        .map_err(Error::Args)?;
    let reason = optional_string(args, "status_reason")?
        .map(|s| s.parse::<StatusReason>())
        .transpose()
        .map_err(Error::Args)?;
    let message = optional_string(args, "message")?;
    let author = format!("scanner:{}", ctx.scanner_name);
    let db = ctx.db.lock().unwrap();
    let target = issue::get(&db, id).map_err(|e| Error::Db(e.to_string()))?;
    issue::set_status(&db, &target.id, status, reason, &author, message.as_deref())
        .map_err(|e| Error::Db(e.to_string()))
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct NotesQuery {
    #[rune(skip)]
    scope: NotesScope,
    #[rune(skip)]
    name: Option<String>,
    #[rune(skip)]
    names: Vec<String>,
}

enum NotesScope {
    Session(String),
    Scan(String),
    Project(String),
}

#[rune::function(instance, path = notes)]
fn session_notes(session: Ref<Session>) -> NotesQuery {
    NotesQuery {
        scope: NotesScope::Session(session.id.clone()),
        name: None,
        names: Vec::new(),
    }
}

#[rune::function(instance, path = notes)]
fn scan_notes(scan: Ref<Scan>) -> NotesQuery {
    NotesQuery {
        scope: NotesScope::Scan(scan.id.clone()),
        name: None,
        names: Vec::new(),
    }
}

#[rune::function(instance, path = notes)]
fn project_notes(project: Ref<Project>) -> NotesQuery {
    NotesQuery {
        scope: NotesScope::Project(project.path.to_string_lossy().into_owned()),
        name: None,
        names: Vec::new(),
    }
}

impl NotesQuery {
    #[rune::function(instance)]
    fn name(mut self, name: Ref<str>) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    /// Filter to notes matching any of `names` exactly.
    #[rune::function(instance)]
    fn names(mut self, names: Ref<RuneVec>) -> Result<Self, VmError> {
        let mut out = Vec::with_capacity(names.len());
        for v in names.iter() {
            out.push(v.borrow_string_ref()?.to_owned());
        }
        self.names = out;
        Ok(self)
    }
}

pub(crate) fn fetch_notes(q: NotesQuery) -> super::Result<Vec<Note>> {
    let ctx = current_scan_ctx();
    let mut filters = NoteFilters {
        name: q.name,
        names: q.names,
        ..Default::default()
    };
    match q.scope {
        NotesScope::Session(id) => filters.session = Some(id),
        NotesScope::Scan(id) => filters.scan = Some(id),
        NotesScope::Project(path) => filters.project = Some(path),
    }
    let db = ctx.db.lock().unwrap();
    let db_notes = note::find(&db, &filters).map_err(|e| Error::Db(e.to_string()))?;
    Ok(db_notes.into_iter().map(Note::from).collect())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Note {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub author: String,
    #[rune(get)]
    pub created: i64,
    #[rune(get)]
    pub name: String,
    #[rune(get)]
    pub value: Value,

    #[rune(skip)]
    target_db: NoteTarget,
    #[rune(skip)]
    target_obj: OnceLock<Value>,

    #[rune(skip)]
    metadata_raw: Option<String>,
    #[rune(skip)]
    metadata: OnceLock<Value>,
}

impl Note {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Note {{ id: {:?}, name: {:?}, author: {:?}, target: {:?}, value: ",
            self.id,
            self.name,
            self.author,
            self.target_uri(),
        )?;
        self.value.debug_fmt(f)?;
        write!(f, ", created: {} }}", self.created)?;
        Ok(())
    }

    // Metadata is always an object; a NULL column (no metadata) reads
    // back as an empty `#{}`, never None.
    fn get_metadata(&self) -> Value {
        self.metadata
            .get_or_init(|| match &self.metadata_raw {
                Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(v) => super::value::json_to_value(&v),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "note metadata JSON decode failed; exposing raw string"
                        );
                        rune::to_value(s.clone()).unwrap()
                    }
                },
                None => rune::to_value(Object::new()).unwrap(),
            })
            .clone()
    }

    fn get_target(&self) -> Value {
        self.target_obj
            .get_or_init(|| rune::to_value(target_to_rune(&self.target_db)).unwrap())
            .clone()
    }

    /// Advisory target as its URI string, for diagnostics.
    pub(crate) fn target_uri(&self) -> String {
        self.target_db.to_uri()
    }

    /// Session ID the note targets, when it targets a session.
    fn target_session(&self) -> Option<String> {
        match &self.target_db {
            NoteTarget::Session(t) => Some(t.session_id.clone()),
            _ => None,
        }
    }
}

// Mirrors gage_db's NoteTarget so scanners can `match note.target`
// instead of probing a stringly-typed `.kind`.
#[derive(Any)]
#[rune(item = ::gage, name = Target)]
pub(crate) enum RuneNoteTarget {
    #[rune(constructor)]
    Session(#[rune(get)] Object),
    #[rune(constructor)]
    Scan(#[rune(get)] String),
    #[rune(constructor)]
    Project(#[rune(get)] String),
}

fn target_to_rune(t: &NoteTarget) -> RuneNoteTarget {
    match t {
        NoteTarget::Session(s) => {
            let mut obj = Object::new();
            obj.insert(
                alloc::String::try_from("session_id").unwrap(),
                rune::to_value(s.session_id.clone()).unwrap(),
            )
            .unwrap();
            obj.insert(
                alloc::String::try_from("line").unwrap(),
                rune::to_value(s.line.map(i64::from)).unwrap(),
            )
            .unwrap();
            obj.insert(
                alloc::String::try_from("line_end").unwrap(),
                rune::to_value(s.line_end.map(i64::from)).unwrap(),
            )
            .unwrap();
            RuneNoteTarget::Session(obj)
        }
        NoteTarget::Scan(s) => RuneNoteTarget::Scan(s.scan_id.clone()),
        NoteTarget::Project(p) => RuneNoteTarget::Project(p.project_path.clone()),
    }
}

/// How the write treats earlier notes with the same
/// `(name, target, author)`. Nothing in the schema constrains the key;
/// the policy is the writer's stated intent.
enum DuplicatePolicy {
    /// Insert unconditionally; earlier notes coexist.
    Insert,
    /// Overwrite the most recent earlier note and delete any older
    /// ones; insert when none exist.
    Replace,
    /// Keep the most recent earlier note untouched and return it;
    /// insert when none exist.
    Ignore,
}

/// Builder returned by `write_note`. The insert runs when the value is
/// awaited; the policy decides how earlier notes with the same
/// `(name, target, author)` are treated.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct NoteInsert {
    #[rune(skip)]
    args: Object,
    #[rune(skip)]
    policy: DuplicatePolicy,
}

impl NoteInsert {
    fn new(args: Object) -> Self {
        Self {
            args,
            policy: DuplicatePolicy::Insert,
        }
    }

    /// Replace earlier notes this writer wrote with the same name and
    /// target: the most recent is overwritten and returned, older ones
    /// are deleted.
    #[rune::function(instance)]
    fn replace_prev(mut self) -> Self {
        self.policy = DuplicatePolicy::Replace;
        self
    }

    /// Keep the most recent earlier note this writer wrote with the
    /// same name and target: it is returned untouched and nothing is
    /// written.
    #[rune::function(instance)]
    fn keep_prev(mut self) -> Self {
        self.policy = DuplicatePolicy::Ignore;
        self
    }
}

fn do_write_note(q: NoteInsert) -> super::Result<Note> {
    let ctx = current_scan_ctx();
    let n = &q.args;

    let target = match n.get("target") {
        Some(v) => target_from_value(v)?,
        None => return Err(Error::Args("write_note requires 'target'".into())),
    };

    let name = required_string(n, "name")?;
    let value_db = match n.get("value").cloned() {
        Some(v) => value_to_note_value(&v)?,
        None => return Err(Error::Args("write_note requires 'value'".into())),
    };
    let metadata_raw = optional_object_json(n, "metadata")?;
    let author =
        optional_string(n, "author")?.unwrap_or_else(|| format!("scanner:{}", ctx.scanner_name));

    let db_note = DbNote {
        metadata: metadata_raw,
        ..DbNote::new(target, &name, value_db, &author)
    };

    // Debug, not info: the db row is the record; restating written
    // note content in the log is diagnostic detail
    tracing::debug!(
        id = db_note.id,
        name = db_note.name,
        target = ?db_note.target,
        author = db_note.author,
        value = ?db_note.value,
        metadata = db_note.metadata,
        "write_note",
    );
    let db = ctx.db.lock().unwrap();
    match q.policy {
        DuplicatePolicy::Insert => insert_new_note(&db, &ctx.run.scan_id, db_note),
        DuplicatePolicy::Replace => {
            let prevs = note::find_by_key(&db, &db_note.name, &db_note.target, &db_note.author)
                .map_err(|e| Error::Db(e.to_string()))?;
            let Some((latest, stale)) = prevs.split_first() else {
                return insert_new_note(&db, &ctx.run.scan_id, db_note);
            };
            for s in stale {
                note::delete(&db, &s.id).map_err(|e| Error::Db(e.to_string()))?;
            }
            let updated =
                note::replace(&db, &latest.id, &db_note).map_err(|e| Error::Db(e.to_string()))?;
            insert_scan_note(&db, &ctx.run.scan_id, &updated.id, ScanLinkRole::Wrote)
                .map_err(|e| Error::Db(e.to_string()))?;
            Ok(updated.into())
        }
        DuplicatePolicy::Ignore => {
            let prevs = note::find_by_key(&db, &db_note.name, &db_note.target, &db_note.author)
                .map_err(|e| Error::Db(e.to_string()))?;
            match prevs.into_iter().next() {
                Some(prev) => Ok(prev.into()),
                None => insert_new_note(&db, &ctx.run.scan_id, db_note),
            }
        }
    }
}

fn insert_new_note(db: &Connection, scan_id: &str, db_note: DbNote) -> super::Result<Note> {
    note::insert(db, &db_note).map_err(|e| Error::Db(e.to_string()))?;
    insert_scan_note(db, scan_id, &db_note.id, ScanLinkRole::Wrote)
        .map_err(|e| Error::Db(e.to_string()))?;
    Ok(db_note.into())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Issue {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: String,
    #[rune(get)]
    pub title: String,
    #[rune(get)]
    pub description: Option<String>,
    #[rune(get)]
    pub status: String,
    #[rune(get)]
    pub status_reason: Option<String>,
    #[rune(get)]
    pub created: i64,
}

impl Issue {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Issue {{ id: {:?}, name: {:?}, title: {:?}, status: {:?}, \
             status_reason: {:?}, description: {:?}, created: {} }}",
            self.id,
            self.name,
            self.title,
            self.status,
            self.status_reason,
            self.description,
            self.created,
        )?;
        Ok(())
    }
}

impl From<DbIssue> for Issue {
    fn from(db: DbIssue) -> Self {
        Self {
            id: db.id,
            name: db.name,
            title: db.title,
            description: db.description,
            status: db.status.as_str().to_string(),
            status_reason: db.status_reason.map(|r| r.as_str().to_string()),
            created: db.created,
        }
    }
}

/// How the write treats an earlier issue with the same
/// `(name, author)`. Nothing in the schema constrains the key; the
/// merge policies act on the most recently created match.
enum IssuePolicy {
    /// Insert unconditionally; earlier issues coexist.
    Insert,
    /// Merge into the prior issue: add any new evidence; leave the
    /// issue status unchanged.
    KeepStatus,
    /// Merge into the prior issue: add any new evidence; reopen a
    /// closed issue when incoming evidence is newer than recorded
    /// evidence of the same name.
    OpenOnNewEvidence,
    /// Merge into the prior issue: add any new evidence; reopen a
    /// closed issue when incoming evidence differs (by digest) from the
    /// latest recorded evidence of the same name.
    OpenOnChangedEvidence,
}

/// Builder returned by `write_issue`. The insert runs when the value is
/// awaited; the policy decides how an earlier issue with the same
/// `(name, author)` is treated. The merge policies insert normally when
/// no earlier issue exists.
#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct IssueInsert {
    #[rune(skip)]
    args: Object,
    #[rune(skip)]
    policy: IssuePolicy,
}

impl IssueInsert {
    fn new(args: Object) -> Self {
        Self {
            args,
            policy: IssuePolicy::Insert,
        }
    }

    /// Merge into the prior issue this writer wrote with the same name:
    /// add new evidence and leave the issue status as-is.
    #[rune::function(instance)]
    fn keep_status(mut self) -> Self {
        self.policy = IssuePolicy::KeepStatus;
        self
    }

    /// Merge into the prior issue this writer wrote with the same name:
    /// add new evidence and reopen a closed issue when the incoming
    /// evidence is newer than recorded evidence of the same name.
    #[rune::function(instance)]
    fn open_on_new_evidence(mut self) -> Self {
        self.policy = IssuePolicy::OpenOnNewEvidence;
        self
    }

    /// Merge into the prior issue this writer wrote with the same name:
    /// add new evidence and reopen a closed issue when the incoming
    /// evidence differs from the latest recorded evidence of the same
    /// name.
    #[rune::function(instance)]
    fn open_on_changed_evidence(mut self) -> Self {
        self.policy = IssuePolicy::OpenOnChangedEvidence;
        self
    }
}

fn do_write_issue(q: IssueInsert) -> super::Result<Issue> {
    let ctx = current_scan_ctx();
    let t = &q.args;

    let name = required_string(t, "name")?;
    let title = required_string(t, "title")?;
    let description = optional_string(t, "description")?;
    let pending = optional_bool(t, "pending")?.unwrap_or(false);
    let author =
        optional_string(t, "author")?.unwrap_or_else(|| format!("scanner:{}", ctx.scanner_name));

    let now = gage_core::datetime::now_ms();
    let evidence = match t.get("evidence") {
        Some(v) => evidence_from_value(v, now)?,
        None => Vec::new(),
    };
    let sessions = optional_string_array(t, "sessions")?;
    let session_ids = related_sessions(&sessions, &evidence);

    let db_issue = DbIssue {
        id: gage_core::uuid::new_uuid(),
        name,
        title,
        description,
        status: if pending {
            IssueStatus::Pending
        } else {
            IssueStatus::Open
        },
        status_reason: None,
        created: now,
        modified: None,
        author,
        target: None,
        metadata: None,
        scan: None,
    };

    tracing::info!(
        id = db_issue.id,
        name = db_issue.name,
        author = db_issue.author,
        "write_issue",
    );
    let db = ctx.db.lock().unwrap();
    let prev = match q.policy {
        IssuePolicy::Insert => None,
        _ => issue::latest_by_key(&db, &db_issue.name, &db_issue.author)
            .map_err(|e| Error::Db(e.to_string()))?,
    };
    match prev {
        None => {
            issue::insert(&db, &db_issue).map_err(|e| Error::Db(e.to_string()))?;
            for ev in &evidence {
                issue::insert_issue_evidence(&db, &ev.row(&db_issue.id))
                    .map_err(|e| Error::Db(e.to_string()))?;
            }
            for session_id in &session_ids {
                issue::insert_session_issue(&db, session_id, &db_issue.id)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }
            insert_scan_issue(&db, &ctx.run.scan_id, &db_issue.id, ScanLinkRole::Wrote)
                .map_err(|e| Error::Db(e.to_string()))?;
            Ok(db_issue.into())
        }
        Some(prev) => {
            let existing =
                issue::issue_evidence_for(&db, &prev.id).map_err(|e| Error::Db(e.to_string()))?;

            let reopen = prev.status == IssueStatus::Closed
                && match q.policy {
                    IssuePolicy::OpenOnNewEvidence => has_newer_evidence(&existing, &evidence),
                    IssuePolicy::OpenOnChangedEvidence => {
                        has_changed_evidence(&existing, &evidence)
                    }
                    _ => false,
                };

            // Add new evidence only; an already-linked note (by id) is a
            // no-op. `seen` also guards intra-batch duplicate note ids.
            let mut seen: HashSet<String> = existing.iter().map(|e| e.note_id.clone()).collect();
            let mut added_evidence = false;
            for ev in &evidence {
                if seen.insert(ev.note_id.clone()) {
                    issue::insert_issue_evidence(&db, &ev.row(&prev.id))
                        .map_err(|e| Error::Db(e.to_string()))?;
                    added_evidence = true;
                }
            }

            let mut added_session = false;
            for session_id in &session_ids {
                if issue::insert_session_issue(&db, session_id, &prev.id)
                    .map_err(|e| Error::Db(e.to_string()))?
                {
                    added_session = true;
                }
            }

            if reopen {
                issue::set_status(&db, &prev.id, IssueStatus::Open, None, &prev.author, None)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }

            if added_evidence || added_session || reopen {
                insert_scan_issue(&db, &ctx.run.scan_id, &prev.id, ScanLinkRole::Carried)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }

            let mut result: Issue = prev.into();
            if reopen {
                result.status = IssueStatus::Open.as_str().to_string();
                result.status_reason = None;
            }
            Ok(result)
        }
    }
}

/// Sessions related to an issue write: the explicit `sessions` list plus
/// the session each evidence note targets, deduplicated in order.
fn related_sessions(explicit: &[String], evidence: &[EvidenceSpec]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    explicit
        .iter()
        .cloned()
        .chain(evidence.iter().filter_map(|ev| ev.session_id.clone()))
        .filter(|s| seen.insert(s.clone()))
        .collect()
}

/// One evidence entry parsed from `write_issue`'s `evidence` list.
struct EvidenceSpec {
    note_id: String,
    name: String,
    timestamp: i64,
    digest: Option<String>,
    /// Session the evidence note targets, when it targets one.
    session_id: Option<String>,
}

impl EvidenceSpec {
    fn row(&self, issue_id: &str) -> DbIssueEvidence {
        DbIssueEvidence {
            issue_id: issue_id.to_string(),
            note_id: self.note_id.clone(),
            name: self.name.clone(),
            timestamp: self.timestamp,
            digest: self.digest.clone(),
        }
    }
}

/// True if any incoming evidence is newer than the recorded evidence of
/// the same name. Evidence with a name not yet on the issue counts as new.
fn has_newer_evidence(existing: &[DbIssueEvidence], incoming: &[EvidenceSpec]) -> bool {
    incoming.iter().any(|ev| {
        match existing
            .iter()
            .filter(|e| e.name == ev.name)
            .map(|e| e.timestamp)
            .max()
        {
            None => true,
            Some(max_ts) => ev.timestamp > max_ts,
        }
    })
}

/// True if any incoming evidence differs (by digest) from the latest
/// recorded evidence of the same name. Evidence with a name not yet on the
/// issue counts as changed.
fn has_changed_evidence(existing: &[DbIssueEvidence], incoming: &[EvidenceSpec]) -> bool {
    incoming.iter().any(|ev| {
        match existing
            .iter()
            .filter(|e| e.name == ev.name)
            .max_by_key(|e| e.timestamp)
        {
            None => true,
            Some(latest) => latest.digest != ev.digest,
        }
    })
}

/// Parses `write_issue`'s `evidence` list. Each entry is a `Note` or an
/// object `#{ note, name?, timestamp?, digest? }`: `name` defaults to the
/// note's name, `timestamp` to `now`, `digest` to none.
fn evidence_from_value(v: &Value, now: i64) -> super::Result<Vec<EvidenceSpec>> {
    let items = v
        .borrow_ref::<RuneVec>()
        .map_err(|e| Error::Args(format!("'evidence' must be a list: {e}")))?;
    let mut out = Vec::new();
    for item in items.iter() {
        out.push(evidence_spec_from_value(item, now)?);
    }
    Ok(out)
}

fn evidence_spec_from_value(item: &Value, now: i64) -> super::Result<EvidenceSpec> {
    if let Ok(note) = item.borrow_ref::<Note>() {
        return Ok(EvidenceSpec {
            note_id: note.id.clone(),
            name: note.name.clone(),
            timestamp: now,
            digest: None,
            session_id: note.target_session(),
        });
    }
    let obj_ref = item.borrow_ref::<Object>().map_err(|e| {
        Error::Args(format!(
            "'evidence' entries must be a Note or #{{note, name, timestamp, digest}}: {e}"
        ))
    })?;
    let obj = &*obj_ref;
    let note_val = obj
        .get("note")
        .ok_or_else(|| Error::Args("evidence entry requires 'note'".into()))?;
    let note = note_val
        .borrow_ref::<Note>()
        .map_err(|e| Error::Args(format!("evidence 'note' must be a Note value: {e}")))?;
    Ok(EvidenceSpec {
        note_id: note.id.clone(),
        name: optional_string(obj, "name")?.unwrap_or_else(|| note.name.clone()),
        timestamp: optional_i64(obj, "timestamp")?.unwrap_or(now),
        digest: optional_string(obj, "digest")?,
        session_id: note.target_session(),
    })
}

/// Build a `NoteTarget` from a `target` object by inferring the variant
/// from which fields are present — no `kind` discriminator:
///
/// - `session` (+ optional `line` / `line_end`) → session target
/// - `scan` → scan target
/// - `project` → project target
///
/// `line_end` requires `line`. Fields from more than one group, or
/// fields we don't recognize, are errors. The target field is required
/// at the call site; this function rejects empty / unspecified objects.
pub(crate) fn target_from_value(v: &Value) -> super::Result<NoteTarget> {
    let obj_ref = v
        .borrow_ref::<Object>()
        .map_err(|e| Error::Args(format!("target must be an object: {e}")))?;
    let obj = &*obj_ref;

    let present = |key: &str| obj.get(key).is_some();
    let session_group = present("session") || present("line") || present("line_end");
    let scan_group = present("scan");
    let project_group = present("project");

    let groups = [session_group, scan_group, project_group]
        .into_iter()
        .filter(|set| *set)
        .count();
    if groups > 1 {
        return Err(Error::Args(
            "ambiguous target: fields name more than one target type".into(),
        ));
    }

    if scan_group {
        return Ok(NoteTarget::Scan(ScanTarget {
            scan_id: required_string(obj, "scan")?,
        }));
    }
    if project_group {
        return Ok(NoteTarget::Project(ProjectTarget {
            project_path: required_string(obj, "project")?,
        }));
    }
    if session_group {
        let line = optional_u32(obj, "line")?;
        let line_end = optional_u32(obj, "line_end")?;
        if line_end.is_some() && line.is_none() {
            return Err(Error::Args("target.line_end requires target.line".into()));
        }
        let session_id = optional_string(obj, "session")?.ok_or_else(|| {
            Error::Args("target with 'line' or 'line_end' requires 'session'".into())
        })?;
        return Ok(NoteTarget::Session(SessionTarget {
            session_id,
            line,
            line_end,
        }));
    }

    Err(Error::Args(
        "target must name a session, scan, or project".into(),
    ))
}

impl From<DbNote> for Note {
    fn from(db: DbNote) -> Self {
        let value = super::value::json_to_value(&db.value.0);

        Self {
            id: db.id,
            author: db.author,
            created: db.created,
            name: db.name,
            value,
            target_db: db.target,
            target_obj: OnceLock::new(),
            metadata_raw: db.metadata,
            metadata: OnceLock::new(),
        }
    }
}

fn required_string(obj: &Object, key: &str) -> super::Result<String> {
    match obj.get(key) {
        None => Err(Error::Args(format!("missing required field '{key}'"))),
        Some(v) => v
            .borrow_string_ref()
            .map(|s| s.to_string())
            .map_err(|e| Error::Args(format!("field '{key}' must be a string: {e}"))),
    }
}

fn optional_string(obj: &Object, key: &str) -> super::Result<Option<String>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => v
            .borrow_string_ref()
            .map(|s| Some(s.to_string()))
            .map_err(|e| Error::Args(format!("field '{key}' must be a string: {e}"))),
    }
}

fn optional_string_array(obj: &Object, key: &str) -> super::Result<Vec<String>> {
    let Some(v) = obj.get(key) else {
        return Ok(Vec::new());
    };
    let items = v
        .borrow_ref::<RuneVec>()
        .map_err(|e| Error::Args(format!("field '{key}' must be a list: {e}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        let s = item
            .borrow_string_ref()
            .map_err(|e| Error::Args(format!("'{key}' entries must be strings: {e}")))?;
        out.push(s.to_string());
    }
    Ok(out)
}

fn optional_bool(obj: &Object, key: &str) -> super::Result<Option<bool>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .map_err(|e| Error::Args(format!("field '{key}' must be a bool: {e}"))),
    }
}

fn optional_i64(obj: &Object, key: &str) -> super::Result<Option<i64>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_integer::<i64>()
            .map(Some)
            .map_err(|e| Error::Args(format!("field '{key}' must be an integer: {e}"))),
    }
}

fn optional_u32(obj: &Object, key: &str) -> super::Result<Option<u32>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => {
            let i = v
                .as_integer::<i64>()
                .map_err(|e| Error::Args(format!("field '{key}' must be an integer: {e}")))?;
            u32::try_from(i).map(Some).map_err(|e| {
                Error::Args(format!(
                    "field '{key}' must be a non-negative integer fitting in u32: {e}"
                ))
            })
        }
    }
}

fn optional_object_json(obj: &Object, key: &str) -> super::Result<Option<String>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => {
            let json = crate::value::value_to_json(v)
                .map_err(|e| Error::Args(format!("field '{key}' could not be serialized: {e}")))?;
            if !json.is_object() {
                return Err(Error::Args(format!("field '{key}' must be an object")));
            }
            Ok(Some(json.to_string()))
        }
    }
}

fn value_to_note_value(v: &Value) -> super::Result<NoteValue> {
    let json = crate::value::value_to_json(v)
        .map_err(|e| Error::Args(format!("field 'value' could not be serialized: {e}")))?;
    Ok(NoteValue(json))
}

#[cfg(test)]
mod tests {
    use rune::Vm;
    use rune::sync::Arc;

    use super::*;

    /// Runs `expr` as the body of `main` with the db module installed
    /// and returns the result value.
    fn eval(expr: &str) -> Result<Value, rune::runtime::VmError> {
        let context = crate::lsp_context().unwrap();
        let runtime = Arc::try_new(context.runtime().unwrap()).unwrap();

        let mut sources = rune::Sources::new();
        sources
            .insert(rune::Source::memory(format!("pub fn main() {{ {expr} }}")).unwrap())
            .unwrap();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .build()
            .unwrap();
        let mut vm = Vm::new(runtime, Arc::try_new(unit).unwrap());
        vm.call(["main"], ())
    }

    // Regression: a registered function whose closure declares an owned
    // Any parameter (Object, String, ...) makes the VM take the
    // argument out of its cell. A script that passed a variable then
    // fails its next read with "Cannot read, value has snapshot
    // M-000000". Registration signatures must borrow.
    #[test]
    fn write_note_leaves_caller_object_readable() {
        let val = eval(
            "let spec = #{ name: \"finding\" };
             let b = gage::write_note(spec);
             spec.name",
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "finding");
    }

    #[test]
    fn write_issue_leaves_caller_object_readable() {
        let val = eval(
            "let spec = #{ title: \"t\" };
             let b = gage::write_issue(spec);
             spec.title",
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "t");
    }

    #[test]
    fn update_issue_leaves_caller_values_readable() {
        let val = eval(
            "let id = \"i1\";
             let args = #{ status: \"open\" };
             let f = gage::update_issue(id, args);
             id + args.status",
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "i1open");
    }

    /// Like `eval`, but the script body runs as `main(a)` with `arg`
    /// bound to `a`.
    fn eval_arg(expr: &str, arg: Value) -> Result<Value, rune::runtime::VmError> {
        let context = crate::lsp_context().unwrap();
        let runtime = Arc::try_new(context.runtime().unwrap()).unwrap();

        let mut sources = rune::Sources::new();
        sources
            .insert(rune::Source::memory(format!("pub fn main(a) {{ {expr} }}")).unwrap())
            .unwrap();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .build()
            .unwrap();
        let mut vm = Vm::new(runtime, Arc::try_new(unit).unwrap());
        vm.call(["main"], (arg,))
    }

    fn test_session_value() -> Value {
        rune::to_value(crate::scan::Session {
            id: "s1".to_string(),
            modified: crate::datetime::DateTime::from_millis(0),
            src: std::path::PathBuf::from("/tmp/session.jsonl"),
            range: None,
        })
        .unwrap()
    }

    #[test]
    fn issues_query_status_leaves_caller_string_readable() {
        let val = eval(
            "let s = \"pending\";
             let q = gage::global().issues().status(s);
             s",
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "pending");
    }

    #[test]
    fn issues_query_name_leaves_caller_string_readable() {
        let val = eval(
            "let n = \"findings\";
             let q = gage::global().issues().name(n);
             n",
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "findings");
    }

    #[test]
    fn notes_query_name_leaves_caller_string_readable() {
        let val = eval_arg(
            "let n = \"finding\";
             let q = a.notes().name(n);
             n",
            test_session_value(),
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "finding");
    }

    #[test]
    fn notes_query_names_leaves_caller_list_readable() {
        let val = eval_arg(
            "let ns = [\"finding\", \"summary\"];
             let q = a.notes().names(ns);
             ns[0]",
            test_session_value(),
        )
        .unwrap();
        assert_eq!(&*val.borrow_string_ref().unwrap(), "finding");
    }

    fn existing(
        note_id: &str,
        name: &str,
        timestamp: i64,
        digest: Option<&str>,
    ) -> DbIssueEvidence {
        DbIssueEvidence {
            issue_id: "issue-1".to_string(),
            note_id: note_id.to_string(),
            name: name.to_string(),
            timestamp,
            digest: digest.map(str::to_string),
        }
    }

    fn incoming(note_id: &str, name: &str, timestamp: i64, digest: Option<&str>) -> EvidenceSpec {
        EvidenceSpec {
            note_id: note_id.to_string(),
            name: name.to_string(),
            timestamp,
            digest: digest.map(str::to_string),
            session_id: None,
        }
    }

    #[test]
    fn newer_evidence_of_unseen_name_is_new() {
        let existing = [existing("n1", "a", 100, None)];
        let inc = [incoming("n2", "b", 50, None)];
        assert!(has_newer_evidence(&existing, &inc));
    }

    #[test]
    fn newer_evidence_compares_only_like_names() {
        let existing = [existing("n1", "a", 100, None)];
        // Higher timestamp but a different name from the only 'a' entry;
        // 'a' itself is older, so nothing newer for 'a'.
        let inc = [incoming("n2", "a", 100, None)];
        assert!(!has_newer_evidence(&existing, &inc));

        let inc_newer = [incoming("n2", "a", 101, None)];
        assert!(has_newer_evidence(&existing, &inc_newer));
    }

    #[test]
    fn changed_evidence_compares_to_latest_like_name() {
        // Two 'a' entries; the latest (ts 200) has digest "y"
        let existing = [
            existing("n1", "a", 100, Some("x")),
            existing("n2", "a", 200, Some("y")),
        ];
        // Same digest as the latest → not changed
        assert!(!has_changed_evidence(
            &existing,
            &[incoming("n3", "a", 300, Some("y"))]
        ));
        // Different digest from the latest → changed (even matching an older one)
        assert!(has_changed_evidence(
            &existing,
            &[incoming("n3", "a", 300, Some("x"))]
        ));
    }

    #[test]
    fn changed_evidence_of_unseen_name_is_changed() {
        let existing = [existing("n1", "a", 100, Some("x"))];
        assert!(has_changed_evidence(
            &existing,
            &[incoming("n2", "b", 50, Some("x"))]
        ));
    }

    fn test_note() -> Note {
        Note::from(DbNote {
            id: "n1".to_string(),
            author: "tester".to_string(),
            created: 0,
            modified: None,
            target: NoteTarget::Session(SessionTarget {
                session_id: "s1".to_string(),
                line: None,
                line_end: None,
            }),
            name: "finding".to_string(),
            value: NoteValue(serde_json::Value::from("x")),
            metadata: None,
            scan: None,
        })
    }

    // Regression: evidence parsing must borrow the caller's list and
    // entries, not take them. A take guts the cell shared with the
    // script's variable, so a later read fails with "Cannot read, value
    // has snapshot M-000000".
    #[test]
    fn evidence_parsing_leaves_caller_values_readable() {
        let note_val = rune::to_value(test_note()).unwrap();
        let list = RuneVec::try_from(vec![note_val.clone()]).unwrap();
        let list_val = rune::to_value(list).unwrap();

        let specs = evidence_from_value(&list_val, 42).unwrap();
        assert_eq!(specs.len(), 1);
        let spec = specs.first().unwrap();
        assert_eq!(spec.note_id, "n1");
        assert_eq!(spec.timestamp, 42);

        assert_eq!(list_val.borrow_ref::<RuneVec>().unwrap().len(), 1);
        assert_eq!(note_val.borrow_ref::<Note>().unwrap().id, "n1");
    }

    #[test]
    fn target_parsing_leaves_caller_value_readable() {
        let mut obj = Object::new();
        obj.insert(
            rune::alloc::String::try_from("session").unwrap(),
            rune::to_value("s1".to_string()).unwrap(),
        )
        .unwrap();
        let obj_val = rune::to_value(obj).unwrap();

        let target = target_from_value(&obj_val).unwrap();
        assert!(matches!(
            target,
            NoteTarget::Session(SessionTarget { ref session_id, .. }) if session_id == "s1"
        ));

        assert!(
            obj_val
                .borrow_ref::<Object>()
                .unwrap()
                .get("session")
                .is_some()
        );
    }
}
