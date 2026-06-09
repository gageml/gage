use std::collections::HashSet;
use std::sync::OnceLock;

use gage_db::issue::{
    self, Issue as DbIssue, IssueError, IssueEvidence as DbIssueEvidence, IssueStatus,
};
use gage_db::note::{self, Note as DbNote, NoteError, NoteFilters, NoteValue};
use gage_db::target::{NoteTarget, ProjectTarget, ScanTarget, SessionTarget};
use rune::Any;
use rune::alloc;
use rune::runtime::{FromValue, Object, Protocol, Ref, Value, Vec as RuneVec};
use rune::{ContextError, Module};
use tracing::warn;

use crate::runtime::error::Error;
use crate::runtime::scan::{Scan, Session};
use crate::runtime::state::{TaskTarget, current_scan_ctx};

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("write_note", |n: Object| NoteInsert::new(n))
        .build()?;
    m.function_meta(NoteInsert::replace_prev)?;
    m.function_meta(NoteInsert::keep_prev)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NoteInsert| async move {
        do_write_note(q)
    })?;
    m.associated_function("replace", |note: Note| async move { do_replace_note(note) })?;

    m.function("write_issue", |t: Object| IssueInsert::new(t))
        .build()?;
    m.function_meta(IssueInsert::keep_status)?;
    m.function_meta(IssueInsert::open_on_new_evidence)?;
    m.function_meta(IssueInsert::open_on_changed_evidence)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: IssueInsert| async move {
        do_write_issue(q)
    })?;

    m.function_meta(session_notes)?;
    m.function_meta(cohort_notes)?;
    m.function_meta(NotesQuery::with_name)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NotesQuery| async move {
        do_fetch_notes(q)
    })?;

    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<Note>()?;
    m.field_function(&Protocol::GET, "metadata", Note::get_metadata)?;
    m.field_function(&Protocol::GET, "target", Note::get_target)?;
    m.ty::<NoteInsert>()?;
    m.ty::<RuneNoteTarget>()?;
    m.ty::<NotesQuery>()?;
    m.ty::<Issue>()?;
    m.ty::<IssueInsert>()?;
    Ok(())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct NotesQuery {
    #[rune(skip)]
    session_ids: Vec<String>,
    #[rune(skip)]
    name: Option<String>,
}

#[rune::function(instance, path = notes)]
fn session_notes(session: Ref<Session>) -> NotesQuery {
    NotesQuery {
        session_ids: vec![session.id.clone()],
        name: None,
    }
}

#[rune::function(instance, path = notes)]
fn cohort_notes(cohort: Ref<Scan>) -> NotesQuery {
    NotesQuery {
        session_ids: cohort.session_ids(),
        name: None,
    }
}

impl NotesQuery {
    #[rune::function(instance)]
    fn with_name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }
}

fn do_fetch_notes(q: NotesQuery) -> super::Result<Vec<Note>> {
    let ctx = current_scan_ctx();
    let filters = NoteFilters {
        sessions: q.session_ids,
        name: q.name,
        ..Default::default()
    };
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
    #[rune(get)]
    pub explanation: Option<String>,

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

/// What a duplicate `(name, target, author)` does at write time.
enum DuplicatePolicy {
    /// Surface the conflict as `Err(Error::Duplicate { prev, new })`.
    Error,
    /// Overwrite the existing note with the new one, return it.
    Replace,
    /// Keep the existing note untouched, return it.
    Ignore,
}

/// Builder returned by `write_note`. The insert runs when the value is
/// awaited; the policy decides what happens on a duplicate.
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
            policy: DuplicatePolicy::Error,
        }
    }

    /// On a duplicate, overwrite the existing note with this one and
    /// return the updated note.
    #[rune::function(instance)]
    fn replace_prev(mut self) -> Self {
        self.policy = DuplicatePolicy::Replace;
        self
    }

    /// On a duplicate, keep the existing note untouched and return it.
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
        Some(v) => target_from_value(v, &ctx.target, &ctx.run.scan_id)?,
        None => default_target(&ctx.target, &ctx.run.scan_id),
    };

    let name = required_string(n, "name")?;
    let value_db = match n.get("value").cloned() {
        Some(v) => value_to_note_value(&v)?,
        None => return Err(Error::Args("write_note requires 'value'".into())),
    };
    let explanation = optional_string(n, "explanation")?;
    let metadata_raw = optional_object_json(n, "metadata")?;

    let db_note = DbNote {
        id: gage_core::uuid::new_uuid(),
        author: format!("scanner:{}", ctx.scanner_name),
        created: gage_core::datetime::now_ms(),
        modified: None,
        target,
        name,
        value: value_db,
        explanation,
        metadata: metadata_raw,
    };

    tracing::info!(
        id = db_note.id,
        name = db_note.name,
        target = ?db_note.target,
        author = db_note.author,
        value = ?db_note.value,
        explanation = db_note.explanation,
        metadata = db_note.metadata,
        "write_note",
    );
    let db = ctx.db.lock().unwrap();
    match note::insert(&db, &db_note) {
        Ok(()) => Ok(db_note.into()),
        Err(NoteError::Duplicate(prev)) => match q.policy {
            DuplicatePolicy::Replace => {
                let updated =
                    note::replace(&db, &prev.id, &db_note).map_err(|e| Error::Db(e.to_string()))?;
                Ok(updated.into())
            }
            DuplicatePolicy::Ignore => Ok((*prev).into()),
            DuplicatePolicy::Error => {
                // `new` carries prev's id so `new.replace()` targets the
                // existing row, per the duplicate-key contract.
                let mut new_db = db_note;
                new_db.id = prev.id.clone();
                let prev_note: Note = (*prev).into();
                let new_note: Note = new_db.into();
                Err(Error::Duplicate {
                    prev: rune::to_value(prev_note).map_err(|e| Error::Db(e.to_string()))?,
                    new: rune::to_value(new_note).map_err(|e| Error::Db(e.to_string()))?,
                })
            }
        },
        Err(e) => Err(Error::Db(e.to_string())),
    }
}

/// Commit a replace of the existing note row this `Note` identifies.
/// Used to resolve `Err(Error::Duplicate { new, .. })` by hand:
/// `new.replace().await`. Identity (`new.id`) is unchanged; only
/// `value`/`metadata`/`explanation` and `modified` are written.
fn do_replace_note(note: Note) -> super::Result<Note> {
    let ctx = current_scan_ctx();
    let db_note = DbNote {
        id: note.id.clone(),
        author: note.author.clone(),
        created: note.created,
        modified: None,
        target: note.target_db.clone(),
        name: note.name.clone(),
        value: value_to_note_value(&note.value)?,
        explanation: note.explanation.clone(),
        metadata: note.metadata_raw.clone(),
    };
    let db = ctx.db.lock().unwrap();
    let updated = note::replace(&db, &note.id, &db_note).map_err(|e| Error::Db(e.to_string()))?;
    Ok(updated.into())
}

#[derive(Any)]
#[rune(item = ::gage)]
pub(crate) struct Issue {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: String,
    #[rune(get)]
    pub target: String,
    #[rune(get)]
    pub title: String,
    #[rune(get)]
    pub description: Option<String>,
    #[rune(get)]
    pub status: String,
    #[rune(get)]
    pub closed_reason: Option<String>,
    #[rune(get)]
    pub created: i64,
}

impl From<DbIssue> for Issue {
    fn from(db: DbIssue) -> Self {
        Self {
            id: db.id,
            name: db.name,
            target: db.target,
            title: db.title,
            description: db.description,
            status: db.status.as_str().to_string(),
            closed_reason: db.closed_reason.map(|r| r.as_str().to_string()),
            created: db.created,
        }
    }
}

/// What a duplicate `(name, target)` does at write time.
enum IssuePolicy {
    /// Surface the conflict as `Err(Error::Duplicate { prev, new })`.
    Error,
    /// Add any new evidence; leave the issue status unchanged.
    KeepStatus,
    /// Add any new evidence; reopen a closed issue when incoming evidence
    /// is newer than recorded evidence of the same name.
    OpenOnNewEvidence,
    /// Add any new evidence; reopen a closed issue when incoming evidence
    /// differs (by digest) from the latest recorded evidence of the same
    /// name.
    OpenOnChangedEvidence,
}

/// Builder returned by `write_issue`. The insert runs when the value is
/// awaited; the policy decides what happens on a duplicate. With no
/// policy a duplicate `(name, target)` surfaces as
/// `Err(Error::Duplicate { prev, new })`.
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
            policy: IssuePolicy::Error,
        }
    }

    /// On a duplicate, add new evidence and leave the issue status as-is.
    #[rune::function(instance)]
    fn keep_status(mut self) -> Self {
        self.policy = IssuePolicy::KeepStatus;
        self
    }

    /// On a duplicate, add new evidence and reopen a closed issue when the
    /// incoming evidence is newer than recorded evidence of the same name.
    #[rune::function(instance)]
    fn open_on_new_evidence(mut self) -> Self {
        self.policy = IssuePolicy::OpenOnNewEvidence;
        self
    }

    /// On a duplicate, add new evidence and reopen a closed issue when the
    /// incoming evidence differs from the latest recorded evidence of the
    /// same name.
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

    // Omitted target means a global issue (empty string); a structured
    // target uses the same inference as `write_note`.
    let target = match t.get("target") {
        Some(v) => target_from_value(v, &ctx.target, &ctx.run.scan_id)?.to_uri(),
        None => String::new(),
    };

    let now = gage_core::datetime::now_ms();
    let evidence = match t.get("evidence") {
        Some(v) => evidence_from_value(v, now)?,
        None => Vec::new(),
    };

    let db_issue = DbIssue {
        id: gage_core::uuid::new_uuid(),
        name,
        target,
        title,
        description,
        status: IssueStatus::Open,
        closed_reason: None,
        created: now,
        modified: None,
        author: format!("scanner:{}", ctx.scanner_name),
    };

    tracing::info!(
        id = db_issue.id,
        name = db_issue.name,
        target = db_issue.target,
        author = db_issue.author,
        "write_issue",
    );
    let db = ctx.db.lock().unwrap();
    match issue::insert(&db, &db_issue) {
        Ok(()) => {
            for ev in &evidence {
                issue::insert_issue_evidence(&db, &ev.row(&db_issue.id))
                    .map_err(|e| Error::Db(e.to_string()))?;
            }
            Ok(db_issue.into())
        }
        Err(IssueError::Duplicate(prev)) if matches!(q.policy, IssuePolicy::Error) => {
            // `new` carries prev's id so it identifies the existing row,
            // per the duplicate-key contract.
            let mut new_db = db_issue;
            new_db.id = prev.id.clone();
            let prev_issue: Issue = (*prev).into();
            let new_issue: Issue = new_db.into();
            Err(Error::Duplicate {
                prev: rune::to_value(prev_issue).map_err(|e| Error::Db(e.to_string()))?,
                new: rune::to_value(new_issue).map_err(|e| Error::Db(e.to_string()))?,
            })
        }
        Err(IssueError::Duplicate(prev)) => {
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
            for ev in &evidence {
                if seen.insert(ev.note_id.clone()) {
                    issue::insert_issue_evidence(&db, &ev.row(&prev.id))
                        .map_err(|e| Error::Db(e.to_string()))?;
                }
            }

            if reopen {
                issue::reopen(&db, &prev.id, &prev.author, None, now)
                    .map_err(|e| Error::Db(e.to_string()))?;
            }

            let mut result: Issue = (*prev).into();
            if reopen {
                result.status = IssueStatus::Open.as_str().to_string();
                result.closed_reason = None;
            }
            Ok(result)
        }
        Err(e) => Err(Error::Db(e.to_string())),
    }
}

/// One evidence entry parsed from `write_issue`'s `evidence` list.
struct EvidenceSpec {
    note_id: String,
    name: String,
    timestamp: i64,
    digest: Option<String>,
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
    let items: RuneVec = rune::from_value(v.clone())
        .map_err(|e| Error::Args(format!("'evidence' must be a list: {e}")))?;
    let mut out = Vec::new();
    for item in items.iter() {
        out.push(evidence_spec_from_value(item, now)?);
    }
    Ok(out)
}

fn evidence_spec_from_value(item: &Value, now: i64) -> super::Result<EvidenceSpec> {
    if let Ok(note) = rune::from_value::<Ref<Note>>(item.clone()) {
        return Ok(EvidenceSpec {
            note_id: note.id.clone(),
            name: note.name.clone(),
            timestamp: now,
            digest: None,
        });
    }
    let obj: Object = rune::from_value(item.clone()).map_err(|e| {
        Error::Args(format!(
            "'evidence' entries must be a Note or #{{note, name, timestamp, digest}}: {e}"
        ))
    })?;
    let note_val = obj
        .get("note")
        .ok_or_else(|| Error::Args("evidence entry requires 'note'".into()))?;
    let note: Ref<Note> = rune::from_value(note_val.clone())
        .map_err(|e| Error::Args(format!("evidence 'note' must be a Note value: {e}")))?;
    Ok(EvidenceSpec {
        note_id: note.id.clone(),
        name: optional_string(&obj, "name")?.unwrap_or_else(|| note.name.clone()),
        timestamp: optional_i64(&obj, "timestamp")?.unwrap_or(now),
        digest: optional_string(&obj, "digest")?,
    })
}

/// The note target for a task with no explicit `target`: the entity the
/// task itself runs against.
fn default_target(task: &TaskTarget, scan_id: &str) -> NoteTarget {
    match task {
        TaskTarget::Session { info, .. } => {
            NoteTarget::Session(SessionTarget::new(info.id.clone()))
        }
        TaskTarget::Scan => NoteTarget::Scan(ScanTarget {
            scan_id: scan_id.to_string(),
        }),
        TaskTarget::Project(p) => NoteTarget::Project(ProjectTarget {
            project_path: p.path.to_string_lossy().into_owned(),
        }),
    }
}

fn current_session_id(task: &TaskTarget) -> Option<String> {
    match task {
        TaskTarget::Session { info, .. } => Some(info.id.clone()),
        _ => None,
    }
}

/// Build a `NoteTarget` from a `target` object by inferring the variant
/// from which fields are present — no `kind` discriminator:
///
/// - `session` / `line` / `line_end` → session target
/// - `scan` → scan target
/// - `project` → project target
///
/// `line` alone uses the current session id; `line_end` requires `line`.
/// An empty object falls back to the task's context target. Fields from
/// more than one group, or fields we don't recognize, are errors.
fn target_from_value(v: &Value, task: &TaskTarget, scan_id: &str) -> super::Result<NoteTarget> {
    let obj: Object = rune::from_value(v.clone())
        .map_err(|e| Error::Args(format!("target must be an object: {e}")))?;

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
            scan_id: required_string(&obj, "scan")?,
        }));
    }
    if project_group {
        return Ok(NoteTarget::Project(ProjectTarget {
            project_path: required_string(&obj, "project")?,
        }));
    }
    if session_group {
        let line = optional_u32(&obj, "line")?;
        let line_end = optional_u32(&obj, "line_end")?;
        if line_end.is_some() && line.is_none() {
            return Err(Error::Args("target.line_end requires target.line".into()));
        }
        let session_id = match optional_string(&obj, "session")? {
            Some(s) => s,
            None => current_session_id(task).ok_or_else(|| {
                Error::Args("target.line outside a session task requires target.session".into())
            })?,
        };
        return Ok(NoteTarget::Session(SessionTarget {
            session_id,
            line,
            line_end,
        }));
    }

    // No recognized fields: an empty object means "the task's target";
    // anything else is a typo'd or unsupported field set.
    if obj.iter().next().is_none() {
        Ok(default_target(task, scan_id))
    } else {
        Err(Error::Args(
            "unrecognized target fields (expected session, line, line_end, scan, or project)"
                .into(),
        ))
    }
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
            explanation: db.explanation,
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

fn optional_i64(obj: &Object, key: &str) -> super::Result<Option<i64>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => i64::from_value(v.clone())
            .map(Some)
            .map_err(|e| Error::Args(format!("field '{key}' must be an integer: {e}"))),
    }
}

fn optional_u32(obj: &Object, key: &str) -> super::Result<Option<u32>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => {
            let i = i64::from_value(v.clone())
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
            let json = serde_json::to_value(v)
                .map_err(|e| Error::Args(format!("field '{key}' could not be serialized: {e}")))?;
            if !json.is_object() {
                return Err(Error::Args(format!("field '{key}' must be an object")));
            }
            Ok(Some(json.to_string()))
        }
    }
}

fn value_to_note_value(v: &Value) -> super::Result<NoteValue> {
    let json = serde_json::to_value(v)
        .map_err(|e| Error::Args(format!("field 'value' could not be serialized: {e}")))?;
    Ok(NoteValue(json))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Two 'a' entries; the latest (ts 200) has digest "y".
        let existing = [
            existing("n1", "a", 100, Some("x")),
            existing("n2", "a", 200, Some("y")),
        ];
        // Same digest as the latest → not changed.
        assert!(!has_changed_evidence(
            &existing,
            &[incoming("n3", "a", 300, Some("y"))]
        ));
        // Different digest from the latest → changed (even matching an older one).
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
}
