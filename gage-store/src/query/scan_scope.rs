//! The tables of a scan scope: one scan's objects, served from an
//! active scan's directory or from a stored scan object.
//!
//! A [`ScanSource`] names the scan. Every table reads the source on
//! each query, so a table over an active scan sees the notes and
//! issues the scan has written so far. The scan's sessions are the
//! dataset's members at the commits the scan links and come from the
//! store either way; the scan's own notes and issues come from the
//! directory while the scan is active and from the scan object's
//! links once stored; carried notes are store objects in both cases.
//!
//! The tables share their schemas and row builders with the store
//! scope, so a query written against one scope runs against the
//! other. The relation tables the store scope serves as views over
//! the `_link` tables, `scan_note`, `session_note`, and the rest, are
//! plain tables here, built from the same scan.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{BooleanBuilder, Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

use super::attachment::{
    attachment_file_rows, attachment_file_schema, attachment_rows, attachment_schema,
};
use super::batch::{BatchSource, BatchTable, external, unique_prefix_lens};
use super::dataset::{DatasetRow, dataset_rows, dataset_schema};
use super::issue::{IssueRow, issue_event_rows, issue_event_schema, issue_rows, issue_schema};
use super::note::note_rows;
use super::note_doc::{note_doc_rows, note_doc_rows_from_plan, note_doc_schema};
use super::scan::{ScanRow, scan_rows, scan_schema};
use crate::scan_dir::ScanDirLayout;
use crate::{
    AttachmentRecord, DatasetStore, IssueChange, IssueStore, NoteFull, NoteStore, ScanStore,
    SessionRecord, Store, StoreError, TaskAttrs, TaskStatus, url,
};

/// The scan a scope serves.
#[derive(Debug, Clone)]
pub enum ScanSource {
    /// An active scan, by its scan directory
    ScanDir(ScanDirLayout),
    /// A stored scan, by id
    Stored(String),
}

/// One note of the scan: the record, its commit when it has one, and
/// whether the scan carried it rather than wrote it.
pub struct ScopedNote {
    pub note: NoteFull,
    pub commit: Option<String>,
    pub carried: bool,
}

/// One issue of the scan, with its evidence as note ids.
pub struct ScopedIssue {
    pub id: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub status_reason: Option<String>,
    pub author: String,
    pub scan: Option<String>,
    pub key: Option<String>,
    pub evidence: Vec<String>,
    pub changes: Vec<IssueChange>,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub commit: Option<String>,
}

impl ScanSource {
    /// The scan's id.
    pub fn scan_id(&self, store: &Store) -> Result<String> {
        match self {
            ScanSource::ScanDir(dir) => dir.scan_id().map(String::from).ok_or_else(|| {
                external(StoreError::Parse(format!(
                    "scan directory {} has no name",
                    dir.root().display()
                )))
            }),
            ScanSource::Stored(id) => Ok(ScanStore::from(store).get(id).map_err(external)?.id),
        }
    }

    /// The commit of the dataset the scan reads, or `None` without a
    /// dataset.
    pub fn dataset_commit(&self, store: &Store) -> Result<Option<String>> {
        match self {
            ScanSource::ScanDir(dir) => dir.dataset_commit().map_err(io_error),
            ScanSource::Stored(id) => Ok(ScanStore::from(store)
                .get(id)
                .map_err(external)?
                .content
                .dataset),
        }
    }

    /// The scan's sessions: the dataset's members at the commits the
    /// scan links, in member order. Empty without a dataset.
    pub fn members(&self, store: &Store) -> Result<Vec<SessionRecord>> {
        match self.dataset_commit(store)? {
            Some(sha) => DatasetStore::from(store)
                .sessions_at(&sha)
                .map_err(external),
            None => Ok(Vec::new()),
        }
    }

    /// The dataset's attachments at the commits the dataset links.
    pub fn attachments(&self, store: &Store) -> Result<Vec<AttachmentRecord>> {
        match self.dataset_commit(store)? {
            Some(sha) => DatasetStore::from(store)
                .attachments_at(&sha)
                .map_err(external),
            None => Ok(Vec::new()),
        }
    }

    /// The scan's notes: the ones it wrote, then the ones it carried,
    /// each in its own order.
    pub fn notes(&self, store: &Store) -> Result<Vec<ScopedNote>> {
        let notes = NoteStore::from(store);
        let mut out = Vec::new();
        match self {
            ScanSource::ScanDir(dir) => {
                for path in dir.note_dirs().map_err(io_error)? {
                    out.push(ScopedNote {
                        note: notes.read_from_dir(&path).map_err(external)?,
                        commit: None,
                        carried: false,
                    });
                }
                for sha in dir.carried_note_commits().map_err(io_error)? {
                    out.push(ScopedNote {
                        note: notes.at_commit(&sha).map_err(external)?,
                        commit: Some(sha),
                        carried: true,
                    });
                }
            }
            ScanSource::Stored(id) => {
                let content = ScanStore::from(store).get(id).map_err(external)?.content;
                for sha in content.notes {
                    out.push(ScopedNote {
                        note: notes.at_commit(&sha).map_err(external)?,
                        commit: Some(sha),
                        carried: false,
                    });
                }
                for sha in content.notes_carried {
                    out.push(ScopedNote {
                        note: notes.at_commit(&sha).map_err(external)?,
                        commit: Some(sha),
                        carried: true,
                    });
                }
            }
        }
        Ok(out)
    }

    /// The issues the scan wrote.
    pub fn issues(&self, store: &Store) -> Result<Vec<ScopedIssue>> {
        let issues = IssueStore::from(store);
        let mut out = Vec::new();
        match self {
            ScanSource::ScanDir(dir) => {
                for path in dir.issue_dirs().map_err(io_error)? {
                    let r = issues.read_from_dir(&path).map_err(external)?;
                    let modified_ms = r.changes.last().map_or(r.created_ms, |c| c.timestamp_ms);
                    out.push(ScopedIssue {
                        id: r.id,
                        name: r.name,
                        title: r.title,
                        description: r.description,
                        status: r.status.as_str().to_string(),
                        status_reason: None,
                        author: r.author,
                        scan: r.scan,
                        key: r.key,
                        evidence: r.evidence,
                        changes: r.changes,
                        created_ms: r.created_ms,
                        modified_ms,
                        commit: None,
                    });
                }
            }
            ScanSource::Stored(id) => {
                let content = ScanStore::from(store).get(id).map_err(external)?.content;
                for sha in content.issues {
                    let full = issues.at_commit(&sha).map_err(external)?;
                    let mut evidence = Vec::with_capacity(full.evidence.len());
                    for note_sha in &full.evidence {
                        evidence.push(store.read_header(note_sha).map_err(external)?.id);
                    }
                    out.push(ScopedIssue {
                        id: full.id,
                        name: full.name,
                        title: full.title,
                        description: full.description,
                        status: full.status.as_str().to_string(),
                        status_reason: full.status_reason.map(|r| r.as_str().to_string()),
                        author: full.author,
                        scan: full.scan,
                        key: full.key,
                        evidence,
                        changes: full.changes,
                        created_ms: full.created_ms,
                        modified_ms: full.modified_ms,
                        commit: Some(sha),
                    });
                }
            }
        }
        Ok(out)
    }

    /// The scan's plan, from `scan/plan.json`, or `None` for a scan
    /// written without one.
    fn plan(&self, store: &Store) -> Result<Option<serde_json::Value>> {
        match self {
            ScanSource::ScanDir(dir) => match fs::read(dir.plan_file()) {
                Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                    external(StoreError::Parse(format!(
                        "{}: {e}",
                        dir.plan_file().display()
                    )))
                }),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io_error(e)),
            },
            ScanSource::Stored(id) => Ok(ScanStore::from(store)
                .get(id)
                .map_err(external)?
                .content
                .plan),
        }
    }

    /// The scan's own row. An active scan has no markers, no commit,
    /// and no run attributes yet: `started` is when the directory was
    /// created, and the task counts are read from the task records
    /// as they stand.
    fn scan_row(&self, store: &Store) -> Result<ScanRow> {
        let dataset = match self.dataset_commit(store)? {
            Some(sha) => Some((store.read_header(&sha).map_err(external)?.id, sha)),
            None => None,
        };
        match self {
            ScanSource::ScanDir(dir) => {
                let id = self.scan_id(store)?;
                let started_ms = fs::metadata(dir.plan_file())
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                let counts = task_counts(dir)?;
                Ok(ScanRow {
                    id_prefix: id.clone(),
                    id,
                    modified_ms: None,
                    created_ms: None,
                    runtime: None,
                    started_ms,
                    stopped_ms: None,
                    canceled: None,
                    tasks: counts.0,
                    completed: counts.1,
                    failed: counts.2,
                    skipped: counts.3,
                    dataset,
                    commit: None,
                })
            }
            ScanSource::Stored(id) => {
                let record = ScanStore::from(store).get(id).map_err(external)?;
                let id_prefix = record.id.clone();
                Ok(ScanRow::from_record(
                    &record,
                    dataset.map(|(id, _)| id),
                    id_prefix,
                ))
            }
        }
    }
}

/// `(total, completed, failed, skipped)` over the task records of an
/// active scan.
fn task_counts(dir: &ScanDirLayout) -> Result<(i64, i64, i64, i64)> {
    let mut counts = (0, 0, 0, 0);
    let scanners = match fs::read_dir(dir.tasks_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(counts),
        Err(e) => return Err(io_error(e)),
    };
    for scanner in scanners {
        let scanner = scanner.map_err(io_error)?.path();
        if !scanner.is_dir() {
            continue;
        }
        for task in fs::read_dir(&scanner).map_err(io_error)? {
            let attrs = task.map_err(io_error)?.path().join("attrs.json");
            let bytes = match fs::read(&attrs) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_error(e)),
            };
            let attrs: TaskAttrs = serde_json::from_slice(&bytes)
                .map_err(|e| external(StoreError::Parse(format!("{}: {e}", attrs.display()))))?;
            counts.0 += 1;
            match attrs.status {
                TaskStatus::Completed => counts.1 += 1,
                TaskStatus::Failed => counts.2 += 1,
                TaskStatus::Skipped => counts.3 += 1,
                TaskStatus::Pending | TaskStatus::Started | TaskStatus::Canceled => {}
            }
        }
    }
    Ok(counts)
}

fn io_error(e: io::Error) -> datafusion::error::DataFusionError {
    datafusion::error::DataFusionError::External(Box::new(e))
}

/// Builds a table's rows from the store under its lock.
type BuildRows = Box<dyn Fn(&Store) -> Result<RecordBatch> + Send + Sync>;

/// A table whose rows are built from the scan source on each query.
struct ScopedSource {
    schema: SchemaRef,
    build: BuildRows,
}

impl std::fmt::Debug for ScopedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedSource").finish()
    }
}

impl BatchSource for ScopedSource {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        (self.build)(store)
    }
}

fn table(
    store: &Arc<Mutex<Store>>,
    schema: SchemaRef,
    build: impl Fn(&Store) -> Result<RecordBatch> + Send + Sync + 'static,
) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(
        Arc::clone(store),
        Arc::new(ScopedSource {
            schema,
            build: Box::new(build),
        }),
    ))
}

fn utf8(name: &str, nullable: bool) -> Field {
    Field::new(name, DataType::Utf8, nullable)
}

fn ids_table(name_a: &str, name_b: &str) -> SchemaRef {
    Arc::new(Schema::new(vec![utf8(name_a, false), utf8(name_b, false)]))
}

fn id_pairs(schema: SchemaRef, pairs: &[(String, String)]) -> Result<RecordBatch> {
    let mut a = StringBuilder::new();
    let mut b = StringBuilder::new();
    for (x, y) in pairs {
        a.append_value(x);
        b.append_value(y);
    }
    Ok(RecordBatch::try_new(
        schema,
        vec![Arc::new(a.finish()), Arc::new(b.finish())],
    )?)
}

/// `(session_id, lines)` of a note's target when it is a session.
fn session_target(note: &NoteFull) -> Option<(String, Option<String>)> {
    let target = note.target.as_deref()?;
    let parsed = url::parse(target).ok()?;
    (parsed.scheme == "session")
        .then(|| (parsed.body.to_string(), parsed.fragment.map(String::from)))
}

/// Every table of the scan scope other than `session`, `entry`, and
/// `message`, by name.
pub fn scan_scope_tables(
    store: &Arc<Mutex<Store>>,
    source: &ScanSource,
) -> Vec<(&'static str, Arc<dyn TableProvider>)> {
    let src = |s: &ScanSource| s.clone();
    let mut tables: Vec<(&'static str, Arc<dyn TableProvider>)> = Vec::new();

    let s = src(source);
    tables.push((
        "note",
        table(store, super::note::stored_note_schema(), move |store| {
            let notes = s.notes(store)?;
            let prefix_len = unique_prefix_lens(notes.iter().map(|n| n.note.id.clone()).collect());
            note_rows(&notes, &prefix_len)
        }),
    ));

    let s = src(source);
    tables.push((
        "issue",
        table(store, issue_schema(), move |store| {
            let issues = s.issues(store)?;
            let prefix_len = unique_prefix_lens(issues.iter().map(|i| i.id.clone()).collect());
            let rows: Vec<IssueRow> = issues
                .iter()
                .map(|i| {
                    let n = prefix_len.get(&i.id).copied().unwrap_or(i.id.len());
                    IssueRow {
                        id: i.id.clone(),
                        name: i.name.clone(),
                        title: i.title.clone(),
                        description: i.description.clone(),
                        status: i.status.clone(),
                        status_reason: i.status_reason.clone(),
                        author: i.author.clone(),
                        evidence_count: i.evidence.len(),
                        created_ms: i.created_ms,
                        modified_ms: i.modified_ms,
                        scan: i.scan.clone(),
                        key: i.key.clone(),
                        id_prefix: i.id.chars().take(n).collect(),
                        commit: i.commit.clone(),
                    }
                })
                .collect();
            issue_rows(&rows)
        }),
    ));

    let s = src(source);
    tables.push((
        "issue_event",
        table(store, issue_event_schema(), move |store| {
            let issues = s.issues(store)?;
            issue_event_rows(issues.iter().map(|i| (i.id.as_str(), i.changes.as_slice())))
        }),
    ));

    let s = src(source);
    tables.push((
        "scan",
        table(store, scan_schema(), move |store| {
            scan_rows(&[s.scan_row(store)?])
        }),
    ));

    let s = src(source);
    tables.push((
        "note_doc",
        table(store, note_doc_schema(), move |store| {
            let rows = match s.plan(store)? {
                Some(plan) => note_doc_rows_from_plan(&plan),
                None => Vec::new(),
            };
            note_doc_rows(&rows)
        }),
    ));

    let s = src(source);
    tables.push((
        "dataset",
        table(store, dataset_schema(), move |store| {
            let Some(sha) = s.dataset_commit(store)? else {
                return dataset_rows(&[]);
            };
            let header = store.read_header(&sha).map_err(external)?;
            let marker = |name: &str, v: Option<i64>| super::batch::marker(&header.id, name, v);
            dataset_rows(&[DatasetRow {
                id: header.id.clone(),
                modified_ms: marker("modified", header.modified_ms)?,
                created_ms: marker("created", header.created_ms)?,
                id_prefix: header.id.clone(),
                commit: sha,
            }])
        }),
    ));

    let s = src(source);
    tables.push((
        "attachment",
        table(store, attachment_schema(), move |store| {
            attachment_rows(&s.attachments(store)?)
        }),
    ));

    let s = src(source);
    tables.push((
        "attachment_file",
        table(store, attachment_file_schema(), move |store| {
            attachment_file_rows(store, &s.attachments(store)?)
        }),
    ));

    let numbered = |a: &str, num: &str, b: &str| {
        Arc::new(Schema::new(vec![
            utf8(a, false),
            Field::new(num, DataType::Int64, false),
            utf8(b, false),
        ]))
    };
    fn numbered_rows(
        schema: SchemaRef,
        owner: &str,
        ids: impl Iterator<Item = String>,
    ) -> Result<RecordBatch> {
        let mut owners = StringBuilder::new();
        let mut nums = Int64Builder::new();
        let mut members = StringBuilder::new();
        for (i, id) in ids.enumerate() {
            owners.append_value(owner);
            nums.append_value(i as i64 + 1);
            members.append_value(&id);
        }
        Ok(RecordBatch::try_new(
            schema,
            vec![
                Arc::new(owners.finish()),
                Arc::new(nums.finish()),
                Arc::new(members.finish()),
            ],
        )?)
    }

    let s = src(source);
    let schema = numbered("dataset_id", "session_num", "session_id");
    tables.push((
        "dataset_session",
        table(store, Arc::clone(&schema), move |store| {
            let Some(sha) = s.dataset_commit(store)? else {
                return numbered_rows(Arc::clone(&schema), "", std::iter::empty());
            };
            let dataset = store.read_header(&sha).map_err(external)?.id;
            let members = s.members(store)?;
            numbered_rows(
                Arc::clone(&schema),
                &dataset,
                members.into_iter().map(|m| m.id),
            )
        }),
    ));

    let s = src(source);
    let schema = numbered("dataset_id", "attachment_num", "attachment_id");
    tables.push((
        "dataset_attachment",
        table(store, Arc::clone(&schema), move |store| {
            let Some(sha) = s.dataset_commit(store)? else {
                return numbered_rows(Arc::clone(&schema), "", std::iter::empty());
            };
            let dataset = store.read_header(&sha).map_err(external)?.id;
            let attachments = s.attachments(store)?;
            numbered_rows(
                Arc::clone(&schema),
                &dataset,
                attachments.into_iter().map(|a| a.id),
            )
        }),
    ));

    let s = src(source);
    let schema = numbered("scan_id", "session_num", "session_id");
    tables.push((
        "scan_session",
        table(store, Arc::clone(&schema), move |store| {
            let scan = s.scan_id(store)?;
            let members = s.members(store)?;
            numbered_rows(
                Arc::clone(&schema),
                &scan,
                members.into_iter().map(|m| m.id),
            )
        }),
    ));

    let s = src(source);
    let schema: SchemaRef = Arc::new(Schema::new(vec![
        utf8("scan_id", false),
        utf8("note_id", false),
        Field::new("carried", DataType::Boolean, false),
    ]));
    tables.push((
        "scan_note",
        table(store, Arc::clone(&schema), move |store| {
            let scan = s.scan_id(store)?;
            let mut scans = StringBuilder::new();
            let mut notes = StringBuilder::new();
            let mut carrieds = BooleanBuilder::new();
            for n in s.notes(store)? {
                scans.append_value(&scan);
                notes.append_value(&n.note.id);
                carrieds.append_value(n.carried);
            }
            Ok(RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(scans.finish()),
                    Arc::new(notes.finish()),
                    Arc::new(carrieds.finish()),
                ],
            )?)
        }),
    ));

    let s = src(source);
    let schema: SchemaRef = Arc::new(Schema::new(vec![
        utf8("session_id", false),
        utf8("note_id", false),
        utf8("lines", true),
    ]));
    tables.push((
        "session_note",
        table(store, Arc::clone(&schema), move |store| {
            let mut sessions = StringBuilder::new();
            let mut notes = StringBuilder::new();
            let mut lines = StringBuilder::new();
            for n in s.notes(store)? {
                if let Some((session, selection)) = session_target(&n.note) {
                    sessions.append_value(session);
                    notes.append_value(&n.note.id);
                    lines.append_option(selection);
                }
            }
            Ok(RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(sessions.finish()),
                    Arc::new(notes.finish()),
                    Arc::new(lines.finish()),
                ],
            )?)
        }),
    ));

    let s = src(source);
    let schema = ids_table("scan_id", "issue_id");
    tables.push((
        "scan_issue",
        table(store, Arc::clone(&schema), move |store| {
            let scan = s.scan_id(store)?;
            let pairs: Vec<(String, String)> = s
                .issues(store)?
                .into_iter()
                .map(|i| (scan.clone(), i.id))
                .collect();
            id_pairs(Arc::clone(&schema), &pairs)
        }),
    ));

    let s = src(source);
    let schema = ids_table("issue_id", "note_id");
    tables.push((
        "issue_evidence",
        table(store, Arc::clone(&schema), move |store| {
            let mut pairs = Vec::new();
            for i in s.issues(store)? {
                for note in i.evidence {
                    pairs.push((i.id.clone(), note));
                }
            }
            id_pairs(Arc::clone(&schema), &pairs)
        }),
    ));

    let s = src(source);
    let schema = ids_table("session_id", "issue_id");
    tables.push((
        "session_issue",
        table(store, Arc::clone(&schema), move |store| {
            // An issue reaches a session through the notes it cites
            let note_sessions: HashMap<String, String> = s
                .notes(store)?
                .iter()
                .filter_map(|n| session_target(&n.note).map(|(sid, _)| (n.note.id.clone(), sid)))
                .collect();
            let mut pairs = Vec::new();
            for i in s.issues(store)? {
                let mut seen = Vec::new();
                for note in &i.evidence {
                    if let Some(session) = note_sessions.get(note)
                        && !seen.contains(session)
                    {
                        seen.push(session.clone());
                        pairs.push((session.clone(), i.id.clone()));
                    }
                }
            }
            id_pairs(Arc::clone(&schema), &pairs)
        }),
    ));

    tables
}
