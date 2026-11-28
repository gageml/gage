//! Scan objects: `gage::scan 2`, reached through [`ScanStore`].
//!
//! Content is `attrs.json`; `dataset.link`, the scanned dataset's
//! commit, when the scan had one; under `logs/`, the scan's `out`,
//! `err`, and `records`; under `tasks/<scanner>/<task>/`, each task's
//! `attrs.json`; and under `scanners/<scanner>/sourcecode.d/`, the
//! scanner's source files as run, opaque to the store. Tasks have no
//! logs of their own.
//!
//! A scanner's name is `<namespace>/<name>`, so `<scanner>` is two
//! directory levels. Version 1 named a scanner without a namespace,
//! and its `<scanner>` is one level. A scanner name read from a
//! version 1 scan has no namespace.
//!
//! The layout is the same in a active scan's
//! directory and in the store, so one decoder serves both through
//! [`ScanFiles`]: [`DirFiles`] over a directory and the store's own
//! view over a commit. [`ScanStore::create`] imports a scan
//! directory's `scan/` tree as the object's content. Under `watermarks/`, the
//! position each task reached on each object it processed.
//! `notes.link`, `notes_carried.link`, and `issues.link` name the
//! commits of the notes and issues the scan wrote or carried. A task
//! that ran agents holds `agents.link`, the commits of the agent
//! session objects, and under `agents/<session object id>/` each
//! agent's `attrs.json` (its exit code), `stderr`, and `result`, the
//! harness's result message verbatim.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gage_core::uuid::is_id;
use serde::{Deserialize, Serialize};

use crate::dataset::OBJECT_TYPE as DATASET_TYPE;
use crate::git::EntryKind;
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::issue::{IssueStore, OBJECT_TYPE as ISSUE_TYPE};
use crate::note::{NoteStore, OBJECT_TYPE as NOTE_TYPE};
use crate::object::{ObjectTree, require_header_type, require_type};
use crate::scan_dir::ScanDirLayout;
use crate::session::{OBJECT_TYPE as SESSION_TYPE, build_files_tree_inner};
use crate::writer::{TreeInput, mktree, write_blob};
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::scan";
pub(crate) const OBJECT_VERSION: &str = "2";
/// The layout version of a scan directory this build writes
const CURRENT_LAYOUT: u32 = 2;
pub(crate) const INDEXED_ATTRS: &[&str] = &[];
const TASKS_DIR: &str = "tasks";
const SCANNERS_DIR: &str = "scanners";
const SOURCE_DIR: &str = "sourcecode.d";
const ATTRS_FILE: &str = "attrs.json";
const PLAN_FILE: &str = "plan.json";
const DATASET_LINK: &str = "dataset.link";
const NOTES_LINK: &str = "notes.link";
const NOTES_CARRIED_LINK: &str = "notes_carried.link";
const ISSUES_LINK: &str = "issues.link";
const AGENTS_LINK: &str = "agents.link";
const AGENTS_DIR: &str = "agents";
/// The native session id of an agent still running, in its `agents/<id>/`
const NATIVE_ID_FILE: &str = "native_id";
/// The name of the driver running an agent, beside its `native_id`
const DRIVER_FILE: &str = "driver";
const STDERR_FILE: &str = "stderr";
const RESULT_FILE: &str = "result";
/// The blobs an `agents/<id>/` directory may hold
const AGENT_FILES: [&str; 3] = [ATTRS_FILE, STDERR_FILE, RESULT_FILE];
const WATERMARKS_DIR: &str = "watermarks";
const LOGS_DIR: &str = "logs";
/// The blobs `logs/` may hold
pub const LOG_NAMES: [&str; 3] = ["out", "err", "records"];

/// Scan operations over an opened store.
pub struct ScanStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for ScanStore<'a> {
    fn from(store: &'a Store) -> Self {
        ScanStore { store }
    }
}

/// The scan's `attrs.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanAttrs {
    /// The runtime that ran the scan, `<scheme> <version>`, where
    /// the scheme term names the runtime scheme, not the application
    pub runtime: String,
    /// UNIX time millis, run start
    pub started: i64,
    /// UNIX time millis, run end
    pub stopped: i64,
    /// The run was interrupted before every task finished
    pub canceled: bool,
    pub tasks: TaskCounts,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCounts {
    /// Tasks planned for the run
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
}

/// A task's `status`. `Pending` and `Started` occur only in the scan
/// directory while the scan runs; a stored scan carries the terminal
/// statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Started,
    Completed,
    Failed,
    Canceled,
    /// A status this build does not know, written by a newer build.
    /// Read only: no writer accepts it, and it is not terminal.
    #[serde(other)]
    Unknown,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Started => "started",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Canceled => "canceled",
            TaskStatus::Unknown => "unknown",
        }
    }

    /// True for a status a stored scan may carry.
    pub fn is_terminal(self) -> bool {
        !matches!(
            self,
            TaskStatus::Pending | TaskStatus::Started | TaskStatus::Unknown
        )
    }
}

/// A task's `attrs.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAttrs {
    pub status: TaskStatus,
    /// UNIX time millis
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    /// UNIX time millis
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<i64>,
    /// Working time in millis: the span from start to stop less the
    /// spells where every agent the task had called was waiting on
    /// the agent pool. Present once the task has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worked_ms: Option<u64>,
}

/// One task of a scan, decoded from `tasks/<scanner>/<task>/`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanTask {
    pub scanner: String,
    pub task: String,
    pub attrs: TaskAttrs,
    /// The commit SHAs of the agent sessions the task ran, from
    /// `agents.link`, in file order. Empty when it ran none.
    pub agent_sessions: Vec<String>,
    /// The task's agent records, from `agents/<id>/`, in id order
    pub agents: Vec<TaskAgent>,
}

/// One agent a task ran, decoded from `agents/<id>/`. `stderr` and
/// `result` are read through [`ScanStore::agent_file`].
#[derive(Debug, Clone, PartialEq)]
pub struct TaskAgent {
    /// The agent session's object id
    pub id: String,
    pub record: AgentRecord,
}

/// What an agent's directory holds: the native session id and the
/// driver running it while the agent runs, its result once it has
/// ended. The runtime writes the running form at agent start and
/// replaces it at stop. A stored scan holds ended records only.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRecord {
    Running {
        native_id: String,
        /// The name of the driver running the agent; `None` in a
        /// record that names none, whose agent the default driver runs
        driver: Option<String>,
    },
    Ended(AgentAttrs),
}

/// An agent's `attrs.json`: the session-level result the runtime
/// records when the agent ends. Fields after `exit_code` default when
/// absent, as in records an earlier runtime wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentAttrs {
    /// The harness process's exit code; -1 when it was signaled
    pub exit_code: i64,
    /// The harness reported the session as an error, or exited
    /// nonzero without a result
    #[serde(default)]
    pub is_error: bool,
    /// The last turn's stop reason; `eof` when the harness closed its
    /// output without one
    #[serde(default)]
    pub stop_reason: String,
    #[serde(default)]
    pub turns: i64,
    #[serde(default)]
    pub duration_ms: i64,
    /// Spend in USD as the harness reported it; absent when the
    /// harness reported no result or no cost
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// One `watermarks/<oid>/<key>` record: how far the task under `key`
/// got on the object `oid`, at the version of the object the task
/// read. The file holds `<version> <mark>` on one line. The version
/// is the object's commit SHA, or for an attachment its content
/// digest, so a mark follows the files and not the commit. The
/// mark's axis is the object's: lines for a session, 0 or 1 for a
/// whole object such as a note or an attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watermark {
    /// The Gage object id
    pub oid: String,
    pub work_id: String,
    /// The object's commit SHA, or an attachment's content digest
    pub version: String,
    /// The position reached
    pub mark: u64,
}

/// A scan's content, decoded from either location.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanContent {
    pub attrs: ScanAttrs,
    /// The commit SHA of the scanned dataset, from `dataset.link`.
    /// `None` when the scan had no dataset.
    pub dataset: Option<String>,
    /// The commit SHAs of the notes the scan wrote, from `notes.link`,
    /// in file order. Empty when it wrote none.
    pub notes: Vec<String>,
    /// The commit SHAs of the notes the scan carried from prior scans,
    /// from `notes_carried.link`, in file order.
    pub notes_carried: Vec<String>,
    /// The commit SHAs of the issues the scan wrote, from
    /// `issues.link`, in file order. Empty when it wrote none.
    pub issues: Vec<String>,
    /// The records under `watermarks/`, in path order.
    pub watermarks: Vec<Watermark>,
    /// The resolved task plan, from `plan.json`, carried verbatim. The
    /// runtime that wrote it defines its shape. `None` for a scan
    /// written without one.
    pub plan: Option<serde_json::Value>,
    /// The names present under the scan's `logs/`, sorted; each one
    /// of [`LOG_NAMES`]
    pub logs: Vec<String>,
    /// In `tasks/` tree order: by scanner name, then task name
    pub tasks: Vec<ScanTask>,
    /// Scanner name to the paths under its `sourcecode.d/`, sorted.
    /// The paths are opaque names; the runtime that wrote them
    /// defines their meaning.
    pub scanners: BTreeMap<String, Vec<String>>,
}

/// The part of a stored scan that a listing of every scan reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanSummary {
    pub attrs: ScanAttrs,
    /// The commit SHA of the scanned dataset, from `dataset.link`
    pub dataset: Option<String>,
}

/// A scan read from the store: its content plus the object markers.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanRecord {
    pub id: String,
    pub commit_sha: String,
    pub content: ScanContent,
    /// From the `created` blob: the time the record was applied to
    /// the store, unrelated to the scan's `started`.
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// What [`ScanStore::delete_cascade`] tombstoned and what it kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDeleted {
    pub scan: String,
    /// The notes the scan wrote that were tombstoned
    pub notes: Vec<String>,
    /// The issues the scan wrote that were tombstoned
    pub issues: Vec<String>,
    /// The notes the scan wrote that another scan or an issue links
    pub notes_kept: Vec<String>,
    /// The issues the scan wrote that another scan links
    pub issues_kept: Vec<String>,
}

/// A read-only view of a scan's files. Paths are `/`-separated and
/// relative to the scan root; the root itself is `""`.
pub trait ScanFiles {
    /// The bytes of the file at `path`, or `None` when there is none.
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;
    /// The names of the directories directly under `path`, sorted.
    /// Empty when `path` does not exist.
    fn list_dirs(&self, path: &str) -> Result<Vec<String>, StoreError>;
    /// The names of the files directly under `path`, sorted. Empty
    /// when `path` does not exist.
    fn list_files(&self, path: &str) -> Result<Vec<String>, StoreError>;
    /// The scan type version whose layout the files follow.
    fn layout(&self) -> Result<u32, StoreError>;
}

impl ScanStore<'_> {
    /// Write the scan under `dir`, a scan directory `scans/<id>/`: the
    /// directory name is the scan's id and the `scan/` tree is the
    /// object's content. The tree is validated first: every task
    /// status must be terminal, a task directory may hold only
    /// `attrs.json`, `agents.link`, and `agents/`, every linked agent
    /// session must be live, and `dataset.link`, when present, must
    /// name a live dataset commit. Returns the commit SHA.
    pub fn create_from_dir(&self, dir: &Path) -> Result<String, StoreError> {
        let layout = ScanDirLayout::new(dir);
        let id = layout
            .scan_id()
            .expect("a scan directory is named by its id");
        assert!(is_id(id), "a scan directory is named by its id: {id:?}");
        let scan_dir = layout.object_dir();
        let content = ScanContent::from_files(&DirFiles::new(&scan_dir))?;
        for task in &content.tasks {
            if !task.attrs.status.is_terminal() {
                return Err(StoreError::Parse(format!(
                    "task {}:{} has status {}, which is not terminal",
                    task.scanner,
                    task.task,
                    task.attrs.status.as_str()
                )));
            }
            for sha in &task.agent_sessions {
                self.require_live(sha, SESSION_TYPE)?;
            }
        }
        let mut tree = ObjectTree {
            attrs: Some(
                serde_json::to_value(&content.attrs)
                    .map_err(|e| StoreError::Parse(format!("scan attrs encode: {e}")))?,
            ),
            ..ObjectTree::default()
        };
        if let Some(sha) = &content.dataset {
            self.require_live(sha, DATASET_TYPE)?;
            tree.links
                .insert(DATASET_LINK.to_string(), vec![sha.clone()]);
        }
        for (file, shas) in [
            (NOTES_LINK, &content.notes),
            (NOTES_CARRIED_LINK, &content.notes_carried),
        ] {
            if shas.is_empty() {
                continue;
            }
            for sha in shas {
                self.require_live(sha, NOTE_TYPE)?;
            }
            tree.links.insert(file.to_string(), shas.clone());
        }
        if !content.issues.is_empty() {
            for sha in &content.issues {
                self.require_live(sha, ISSUE_TYPE)?;
            }
            tree.links
                .insert(ISSUES_LINK.to_string(), content.issues.clone());
        }
        if let Some(plan) = &content.plan {
            let mut json = serde_json::to_vec(plan)
                .map_err(|e| StoreError::Parse(format!("scan plan encode: {e}")))?;
            json.push(b'\n');
            tree.blobs.insert(PLAN_FILE.to_string(), json);
        }
        if let Some(sha) = import_opaque_tree(self.store.path(), &scan_dir.join(WATERMARKS_DIR))? {
            tree.subtrees.insert(WATERMARKS_DIR.to_string(), sha);
        }
        let logs_dir = scan_dir.join(LOGS_DIR);
        if logs_dir.is_dir() {
            let sha = import_fixed_blobs(self.store.path(), &logs_dir, &LOG_NAMES)?;
            tree.subtrees.insert(LOGS_DIR.to_string(), sha);
        }
        if let Some(sha) = import_tasks_tree(self.store.path(), &scan_dir.join(TASKS_DIR))? {
            tree.subtrees.insert(TASKS_DIR.to_string(), sha);
        }
        if let Some(sha) = import_scanners_tree(self.store.path(), &scan_dir.join(SCANNERS_DIR))? {
            tree.subtrees.insert(SCANNERS_DIR.to_string(), sha);
        }
        self.store
            .create(OBJECT_TYPE, OBJECT_VERSION, id, &tree, "scan")
    }

    /// Look up one scan by full id or unique prefix.
    ///
    /// Returns [`StoreError::ObjectNotFound`] when no object matches,
    /// [`StoreError::AmbiguousId`] when more than one does, and
    /// [`StoreError::WrongType`] when the match is not a scan.
    pub fn get(&self, id_or_prefix: &str) -> Result<ScanRecord, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        self.record(&object.commit_sha)
    }

    /// Read the scan at the given commit SHA.
    pub fn at_commit(&self, commit_sha: &str) -> Result<ScanRecord, StoreError> {
        let object = self.store.read_object(commit_sha)?;
        require_type(&object, OBJECT_TYPE)?;
        self.record(&object.commit_sha)
    }

    /// The run attributes and the dataset link of the scan at
    /// `commit_sha`. Reads those two files only, where
    /// [`ScanStore::at_commit`] decodes the whole scan; a listing over
    /// every scan uses this.
    pub(crate) fn summary_at_commit(&self, commit_sha: &str) -> Result<ScanSummary, StoreError> {
        let files = self.scan_files(commit_sha)?;
        Ok(ScanSummary {
            attrs: read_json(&files, ATTRS_FILE)?,
            dataset: read_dataset_link(&files)?,
        })
    }

    /// The tasks of the scan at `commit_sha`, in `tasks/` tree order,
    /// read without the rest of the scan.
    pub(crate) fn tasks_at_commit(&self, commit_sha: &str) -> Result<Vec<ScanTask>, StoreError> {
        read_tasks(&self.scan_files(commit_sha)?)
    }

    /// The resolved task plan of the scan at `commit_sha`, read
    /// without the rest of the scan. `None` for a scan written
    /// without one.
    pub(crate) fn plan_at_commit(
        &self,
        commit_sha: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        read_plan(&self.scan_files(commit_sha)?)
    }

    /// The files of the scan at `commit_sha`, once its markers name a
    /// scan this build reads.
    fn scan_files<'c>(&'c self, commit_sha: &'c str) -> Result<CommitFiles<'c>, StoreError> {
        let header = self.store.read_header(commit_sha)?;
        require_header_type(&header, OBJECT_TYPE)?;
        Ok(CommitFiles {
            store: self.store,
            commit: commit_sha,
        })
    }

    /// Every live scan, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<ScanRecord, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over scans.
    pub fn query(&self) -> ScanQuery<'_> {
        ScanQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// The bytes of one log of the scan at `commit_sha`: `name` is
    /// one of [`LOG_NAMES`]. `None` when the scan did not produce it.
    pub fn scan_log(&self, commit_sha: &str, name: &str) -> Result<Option<Vec<u8>>, StoreError> {
        CommitFiles {
            store: self.store,
            commit: commit_sha,
        }
        .read(&format!("{LOGS_DIR}/{name}"))
    }

    /// The bytes of one file of a task's agent record at `commit_sha`:
    /// `name` is `stderr` or `result`. `None` when the record lacks it.
    pub fn agent_file(
        &self,
        commit_sha: &str,
        scanner: &str,
        task: &str,
        agent_id: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        CommitFiles {
            store: self.store,
            commit: commit_sha,
        }
        .read(&agent_file_path(scanner, task, agent_id, name))
    }

    /// The bytes of `plan.json`, the resolved task plan of the scan at
    /// `commit_sha`. `None` for a scan written without one.
    pub fn plan_file(&self, commit_sha: &str) -> Result<Option<Vec<u8>>, StoreError> {
        CommitFiles {
            store: self.store,
            commit: commit_sha,
        }
        .read(PLAN_FILE)
    }

    /// The bytes of one source file of a scanner at `commit_sha`, by
    /// its path under `sourcecode.d/`. `None` when there is no such
    /// file.
    pub fn source_file(
        &self,
        commit_sha: &str,
        scanner: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        CommitFiles {
            store: self.store,
            commit: commit_sha,
        }
        .read(&format!("{SCANNERS_DIR}/{scanner}/{SOURCE_DIR}/{path}"))
    }

    /// Delete a scan by writing a parentless tombstone commit. The
    /// notes and issues the scan wrote stay. Returns the resolved id.
    pub fn delete(&self, id_or_prefix: &str) -> Result<String, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        self.store.delete(&object, "scan delete")?;
        Ok(object.header.id)
    }

    /// Delete a scan and the notes and issues it wrote, named by
    /// `notes.link` and `issues.link`, less the ones something else
    /// uses. A note or issue is kept, unchanged, when the current
    /// commit of another live scan or of a live issue links any commit
    /// of it: a keyed issue that two scans wrote, or a note that a
    /// later scan carried. A note's target does not keep the target.
    /// Carried notes stay, since the scan did not write them. A note
    /// or issue deleted since the scan wrote it is skipped.
    ///
    /// The issues go first, so an issue this delete tombstones does
    /// not keep the notes it cites. The scan goes last, so a failure
    /// among the notes and issues leaves the scan in place.
    pub fn delete_cascade(&self, id_or_prefix: &str) -> Result<ScanDeleted, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        let content = ScanContent::from_files(&CommitFiles {
            store: self.store,
            commit: &object.commit_sha,
        })?;
        let scan = object.header.id.clone();

        let issues = IssueStore::from(self.store);
        let (issue_ids, issues_kept) = self.unlinked(&content.issues, &scan)?;
        let mut issues_deleted = Vec::new();
        for id in issue_ids {
            match issues.delete(&id) {
                Ok(id) => issues_deleted.push(id),
                // Already deleted since the scan wrote it
                Err(StoreError::ObjectDeleted(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let notes = NoteStore::from(self.store);
        let (note_ids, notes_kept) = self.unlinked(&content.notes, &scan)?;
        let mut notes_deleted = Vec::new();
        for id in note_ids {
            match notes.delete(&id) {
                Ok(id) => notes_deleted.push(id),
                // Already deleted since the scan wrote it
                Err(StoreError::ObjectDeleted(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.store.delete(&object, "scan delete")?;
        Ok(ScanDeleted {
            scan,
            notes: notes_deleted,
            issues: issues_deleted,
            notes_kept,
            issues_kept,
        })
    }

    /// The ids of the objects the commits `shas` belong to, each once
    /// in link order, split into those nothing else links and those
    /// that a live scan other than `scan` or a live issue links now.
    fn unlinked(
        &self,
        shas: &[String],
        scan: &str,
    ) -> Result<(Vec<String>, Vec<String>), StoreError> {
        let mut ids: Vec<String> = Vec::new();
        for sha in shas {
            let id = self.store.read_header(sha)?.id;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let linked = self
            .store
            .linked_by_tips(&ids, &[OBJECT_TYPE, ISSUE_TYPE], scan)?;
        Ok(ids.into_iter().partition(|id| !linked.contains(id)))
    }

    /// The commit at `sha` must be a live object of `object_type`.
    fn require_live(&self, sha: &str, object_type: &str) -> Result<(), StoreError> {
        let object = self.store.read_object(sha)?;
        require_type(&object, object_type)?;
        if object.header.is_tombstone() {
            return Err(StoreError::ObjectDeleted(object.header.id));
        }
        Ok(())
    }

    fn record(&self, commit_sha: &str) -> Result<ScanRecord, StoreError> {
        let header = self.store.read_header(commit_sha)?;
        let content = ScanContent::from_files(&CommitFiles {
            store: self.store,
            commit: commit_sha,
        })?;
        Ok(ScanRecord {
            id: header.id,
            commit_sha: commit_sha.to_string(),
            content,
            created_ms: header.created_ms.unwrap_or_default(),
            modified_ms: header.modified_ms.unwrap_or_default(),
        })
    }
}

/// A selection over scans: an order and a limit. `iter` reads matching
/// scans one at a time, content included.
pub struct ScanQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> ScanQuery<'a> {
    pub fn order(mut self, order: Order) -> Self {
        self.query.order = order;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.query.limit = Some(limit);
        self
    }

    /// The number of scans the selection matches, ignoring any limit.
    /// Served by the index; no object is read.
    pub fn count(&self) -> Result<usize, StoreError> {
        let unlimited = ObjectQuery {
            limit: None,
            ..self.query.clone()
        };
        Ok(self.store.select(&unlimited)?.len())
    }

    /// The matching tips, in query order, without reading any object.
    pub fn tips(self) -> Result<Vec<SelectedTip>, StoreError> {
        self.store.select(&self.query)
    }

    /// Run the selection.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<ScanRecord, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let tips = store.select(&self.query)?;
        Ok(tips
            .into_iter()
            .map(move |tip| ScanStore::from(store).record(&tip.sha)))
    }
}

impl ScanContent {
    /// Decode a scan from its files. The root `attrs.json` and every
    /// task `attrs.json` are required; `dataset.link`, when present,
    /// must list exactly one SHA.
    pub fn from_files(files: &dyn ScanFiles) -> Result<ScanContent, StoreError> {
        let attrs = read_json(files, ATTRS_FILE)?;
        let dataset = read_dataset_link(files)?;
        let notes = read_link(files, NOTES_LINK)?.unwrap_or_default();
        let notes_carried = read_link(files, NOTES_CARRIED_LINK)?.unwrap_or_default();
        let issues = read_link(files, ISSUES_LINK)?.unwrap_or_default();
        let watermarks = read_watermarks(files)?;
        let plan = read_plan(files)?;
        let logs = files.list_files(LOGS_DIR)?;
        let tasks = read_tasks(files)?;
        let mut scanners = BTreeMap::new();
        for scanner in scanner_names(files, SCANNERS_DIR)? {
            let source = format!("{SCANNERS_DIR}/{scanner}/{SOURCE_DIR}");
            let mut paths = Vec::new();
            walk_files(files, &source, "", &mut paths)?;
            scanners.insert(scanner, paths);
        }
        Ok(ScanContent {
            attrs,
            dataset,
            notes,
            notes_carried,
            issues,
            watermarks,
            plan,
            logs,
            tasks,
            scanners,
        })
    }
}

/// The commit SHA in `dataset.link`, or `None` without the file. The
/// file must list exactly one SHA.
fn read_dataset_link(files: &dyn ScanFiles) -> Result<Option<String>, StoreError> {
    match read_link(files, DATASET_LINK)? {
        None => Ok(None),
        Some(shas) => match shas.as_slice() {
            [sha] => Ok(Some(sha.clone())),
            _ => Err(StoreError::Parse(format!(
                "scan file {DATASET_LINK}: expected one SHA, found {}",
                shas.len()
            ))),
        },
    }
}

/// `plan.json` decoded, or `None` without the file.
fn read_plan(files: &dyn ScanFiles) -> Result<Option<serde_json::Value>, StoreError> {
    match files.read(PLAN_FILE)? {
        Some(bytes) => Ok(Some(parse_json(&bytes, PLAN_FILE)?)),
        None => Ok(None),
    }
}

/// The tasks under `tasks/`, in tree order: by scanner name, then
/// task name. Every task's `attrs.json` is required. Serves an
/// active scan's directory, whose root `attrs.json` does not exist
/// yet, as well as a stored scan.
pub fn read_tasks(files: &dyn ScanFiles) -> Result<Vec<ScanTask>, StoreError> {
    let mut tasks = Vec::new();
    for scanner in scanner_names(files, TASKS_DIR)? {
        let scanner_path = format!("{TASKS_DIR}/{scanner}");
        for task in files.list_dirs(&scanner_path)? {
            let task_path = format!("{scanner_path}/{task}");
            let attrs = read_json(files, &format!("{task_path}/{ATTRS_FILE}"))?;
            let agent_sessions =
                read_link(files, &format!("{task_path}/{AGENTS_LINK}"))?.unwrap_or_default();
            let mut agents = Vec::new();
            for id in files.list_dirs(&format!("{task_path}/{AGENTS_DIR}"))? {
                let dir = format!("{task_path}/{AGENTS_DIR}/{id}");
                let record = read_agent_record(files, &dir)?;
                agents.push(TaskAgent { id, record });
            }
            tasks.push(ScanTask {
                scanner: scanner.clone(),
                task,
                attrs,
                agent_sessions,
                agents,
            });
        }
    }
    Ok(tasks)
}

/// The scanner names under `root`, which is `tasks` or `scanners`, in
/// tree order. A version 1 scan holds each scanner in one directory.
/// Later versions hold it in `<namespace>/<name>`.
fn scanner_names(files: &dyn ScanFiles, root: &str) -> Result<Vec<String>, StoreError> {
    let top = files.list_dirs(root)?;
    if files.layout()? < 2 {
        return Ok(top);
    }
    let mut names = Vec::new();
    for namespace in top {
        for name in files.list_dirs(&format!("{root}/{namespace}"))? {
            names.push(format!("{namespace}/{name}"));
        }
    }
    Ok(names)
}

/// The path of one file of a task's agent record, relative to the
/// scan root: `name` is `stderr` or `result`.
pub fn agent_file_path(scanner: &str, task: &str, agent_id: &str, name: &str) -> String {
    format!("{TASKS_DIR}/{scanner}/{task}/{AGENTS_DIR}/{agent_id}/{name}")
}

/// Every record under `watermarks/`, in path order. A path that is
/// not `<oid>/<key>`, or a content that is not `<version> <mark>`,
/// is an error.
fn read_watermarks(files: &dyn ScanFiles) -> Result<Vec<Watermark>, StoreError> {
    let mut paths = Vec::new();
    walk_files(files, WATERMARKS_DIR, "", &mut paths)?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        let [oid, key] = parts.as_slice() else {
            return Err(StoreError::Parse(format!(
                "scan file {WATERMARKS_DIR}/{path}: expected <oid>/<key>"
            )));
        };
        let bytes = files
            .read(&format!("{WATERMARKS_DIR}/{path}"))?
            .ok_or_else(|| {
                StoreError::Parse(format!("scan file {WATERMARKS_DIR}/{path} is missing"))
            })?;
        let content = String::from_utf8_lossy(&bytes);
        let (version, mark) = parse_watermark(content.trim()).ok_or_else(|| {
            StoreError::Parse(format!(
                "scan file {WATERMARKS_DIR}/{path}: {:?} is not <version> <mark>",
                content.trim()
            ))
        })?;
        out.push(Watermark {
            oid: oid.to_string(),
            work_id: key.to_string(),
            version: version.to_string(),
            mark,
        });
    }
    Ok(out)
}

/// The `(version, mark)` of a watermark file's content, or `None`
/// when it is not a commit SHA or a content digest followed by an
/// integer.
fn parse_watermark(content: &str) -> Option<(&str, u64)> {
    let mut parts = content.split_whitespace();
    let version = parts.next().filter(|s| is_sha(s) || is_digest(s))?;
    let mark = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((version, mark))
}

/// The SHAs in the link file `name`, one per line, or `None` when
/// the file is absent.
/// The record under one `agents/<id>/`: the result when `attrs.json`
/// is present, else the running marker.
fn read_agent_record(files: &dyn ScanFiles, dir: &str) -> Result<AgentRecord, StoreError> {
    let attrs_path = format!("{dir}/{ATTRS_FILE}");
    if let Some(bytes) = files.read(&attrs_path)? {
        return Ok(AgentRecord::Ended(parse_json(&bytes, &attrs_path)?));
    }
    let native_path = format!("{dir}/{NATIVE_ID_FILE}");
    let Some(bytes) = files.read(&native_path)? else {
        return Err(StoreError::Parse(format!(
            "agent record {dir} holds neither {ATTRS_FILE} nor {NATIVE_ID_FILE}"
        )));
    };
    let native_id = String::from_utf8_lossy(&bytes).trim().to_string();
    if native_id.is_empty() {
        return Err(StoreError::Parse(format!(
            "scan file {native_path} is empty"
        )));
    }
    let driver = files
        .read(&format!("{dir}/{DRIVER_FILE}"))?
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string());
    Ok(AgentRecord::Running { native_id, driver })
}

fn read_link(files: &dyn ScanFiles, name: &str) -> Result<Option<Vec<String>>, StoreError> {
    let Some(bytes) = files.read(name)? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut shas = Vec::new();
    for line in text.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if !is_sha(line) {
            return Err(StoreError::Parse(format!(
                "scan file {name}: {line:?} is not a commit SHA"
            )));
        }
        shas.push(line.to_string());
    }
    Ok(Some(shas))
}

/// True for a 40-character lowercase hex SHA, as the store writes them.
fn is_sha(value: &str) -> bool {
    value.len() == 40 && is_lower_hex(value)
}

/// An attachment content digest: 16 lowercase hex digits
fn is_digest(value: &str) -> bool {
    value.len() == 16 && is_lower_hex(value)
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Every file under `root`, as paths relative to it, depth first in
/// name order.
fn walk_files(
    files: &dyn ScanFiles,
    root: &str,
    rel: &str,
    out: &mut Vec<String>,
) -> Result<(), StoreError> {
    let here = if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{rel}")
    };
    let join = |name: &str| {
        if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        }
    };
    for name in files.list_files(&here)? {
        out.push(join(&name));
    }
    for name in files.list_dirs(&here)? {
        walk_files(files, root, &join(&name), out)?;
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(
    files: &dyn ScanFiles,
    path: &str,
) -> Result<T, StoreError> {
    let bytes = files
        .read(path)?
        .ok_or_else(|| StoreError::Parse(format!("scan file {path} is missing")))?;
    parse_json(&bytes, path)
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8], path: &str) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Parse(format!("scan file {path}: {e}")))
}

/// Build the `tasks/` tree from a scan directory's `tasks/`:
/// `<namespace>/<name>/<task>/`. Returns `None` when the directory is
/// absent or holds no scanner.
fn import_tasks_tree(store_path: &Path, tasks_dir: &Path) -> Result<Option<String>, StoreError> {
    import_namespaced_tree(store_path, tasks_dir, |scanner_dir| {
        let mut task_trees: Vec<(String, String)> = Vec::new();
        require_dirs_only(scanner_dir)?;
        for task_dir in subdirs(scanner_dir)? {
            task_trees.push((
                dir_name(&task_dir),
                import_task_tree(store_path, &task_dir)?,
            ));
        }
        tree_of_trees(store_path, &task_trees)
    })
}

/// Build the tree of `root`, which holds one directory per namespace
/// and under each one directory per scanner. `scanner_tree` builds a
/// scanner's tree from its directory. Returns `None` when `root` is
/// absent or holds no scanner.
fn import_namespaced_tree(
    store_path: &Path,
    root: &Path,
    scanner_tree: impl Fn(&Path) -> Result<String, StoreError>,
) -> Result<Option<String>, StoreError> {
    let mut namespace_trees: Vec<(String, String)> = Vec::new();
    require_dirs_only(root)?;
    for namespace_dir in subdirs(root)? {
        let mut scanner_trees: Vec<(String, String)> = Vec::new();
        require_dirs_only(&namespace_dir)?;
        for scanner_dir in subdirs(&namespace_dir)? {
            scanner_trees.push((dir_name(&scanner_dir), scanner_tree(&scanner_dir)?));
        }
        if !scanner_trees.is_empty() {
            namespace_trees.push((
                dir_name(&namespace_dir),
                tree_of_trees(store_path, &scanner_trees)?,
            ));
        }
    }
    if namespace_trees.is_empty() {
        return Ok(None);
    }
    Ok(Some(tree_of_trees(store_path, &namespace_trees)?))
}

/// Build one task's tree: the `attrs.json` and `agents.link` blobs
/// and the `agents/` subtree of per-agent fixed blobs. Anything else
/// is an error.
fn import_task_tree(store_path: &Path, task_dir: &Path) -> Result<String, StoreError> {
    let mut blobs: BTreeMap<String, String> = BTreeMap::new();
    let mut agents_tree: Option<String> = None;
    for entry in fs::read_dir(task_dir).map_err(|e| read_error(task_dir, e))? {
        let entry = entry.map_err(|e| read_error(task_dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() && name == AGENTS_DIR {
            agents_tree = Some(import_agents_tree(store_path, &path)?);
            continue;
        }
        if !path.is_file() || !(name == ATTRS_FILE || name == AGENTS_LINK) {
            return Err(StoreError::InvalidPath {
                path: path.display().to_string(),
                reason: "unexpected entry in a task directory".to_string(),
            });
        }
        let bytes = fs::read(&path).map_err(|e| read_error(&path, e))?;
        blobs.insert(name, write_blob(store_path, &bytes)?);
    }
    let mut entries: Vec<TreeInput<'_>> = blobs
        .iter()
        .map(|(name, sha)| TreeInput {
            mode: "100644",
            sha,
            name,
        })
        .collect();
    if let Some(sha) = &agents_tree {
        entries.push(TreeInput {
            mode: "040000",
            sha,
            name: AGENTS_DIR,
        });
    }
    mktree(store_path, &entries)
}

/// Build a task's `agents/` tree: one subtree per agent session id,
/// each holding only [`AGENT_FILES`]. An agent still running when the
/// scan ended left only its native id; nothing of it is stored, and
/// `agents.link` never named it.
fn import_agents_tree(store_path: &Path, agents_dir: &Path) -> Result<String, StoreError> {
    require_dirs_only(agents_dir)?;
    let mut trees: Vec<(String, String)> = Vec::new();
    for agent_dir in subdirs(agents_dir)? {
        if !agent_dir.join(ATTRS_FILE).is_file() {
            continue;
        }
        trees.push((
            dir_name(&agent_dir),
            import_fixed_blobs(store_path, &agent_dir, &AGENT_FILES)?,
        ));
    }
    tree_of_trees(store_path, &trees)
}

/// Build a tree of blobs from a directory whose files must each be
/// one of `allowed`.
fn import_fixed_blobs(
    store_path: &Path,
    dir: &Path,
    allowed: &[&str],
) -> Result<String, StoreError> {
    let mut blobs: BTreeMap<String, String> = BTreeMap::new();
    for entry in fs::read_dir(dir).map_err(|e| read_error(dir, e))? {
        let entry = entry.map_err(|e| read_error(dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !path.is_file() || !allowed.contains(&name.as_str()) {
            return Err(StoreError::InvalidPath {
                path: path.display().to_string(),
                reason: format!("expected one of {}", allowed.join(", ")),
            });
        }
        let bytes = fs::read(&path).map_err(|e| read_error(&path, e))?;
        blobs.insert(name, write_blob(store_path, &bytes)?);
    }
    let entries: Vec<TreeInput<'_>> = blobs
        .iter()
        .map(|(name, sha)| TreeInput {
            mode: "100644",
            sha,
            name,
        })
        .collect();
    mktree(store_path, &entries)
}

/// Build the `scanners/` tree from a scan directory's `scanners/`:
/// under `<namespace>/<name>/`, one subtree per scanner holding its
/// opaque `sourcecode.d/` tree, imported as it is. Returns `None`
/// when the directory is absent or holds no scanner.
fn import_scanners_tree(
    store_path: &Path,
    scanners_dir: &Path,
) -> Result<Option<String>, StoreError> {
    import_namespaced_tree(store_path, scanners_dir, |scanner_dir| {
        for entry in fs::read_dir(scanner_dir).map_err(|e| read_error(scanner_dir, e))? {
            let path = entry.map_err(|e| read_error(scanner_dir, e))?.path();
            if !path.is_dir() || path.file_name().is_none_or(|n| n != SOURCE_DIR) {
                return Err(StoreError::InvalidPath {
                    path: path.display().to_string(),
                    reason: "unexpected entry in a scanner directory".to_string(),
                });
            }
        }
        let mut entries: Vec<TreeInput<'_>> = Vec::new();
        let source_sha = import_opaque_tree(store_path, &scanner_dir.join(SOURCE_DIR))?;
        if let Some(sha) = &source_sha {
            entries.push(TreeInput {
                mode: "040000",
                sha,
                name: SOURCE_DIR,
            });
        }
        mktree(store_path, &entries)
    })
}

/// Import a directory as an opaque tree: every file beneath it, at its
/// relative path, bytes preserved. `None` when the directory is absent.
pub(crate) fn import_opaque_tree(
    store_path: &Path,
    dir: &Path,
) -> Result<Option<String>, StoreError> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_blobs(store_path, dir, "", &mut entries)?;
    Ok(Some(build_files_tree_inner(store_path, entries)?))
}

fn collect_blobs(
    store_path: &Path,
    dir: &Path,
    rel: &str,
    out: &mut Vec<(String, String)>,
) -> Result<(), StoreError> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| read_error(dir, e))?
        .map(|entry| entry.map(|e| e.path()).map_err(|e| read_error(dir, e)))
        .collect::<Result<_, _>>()?;
    paths.sort();
    for path in paths {
        let name = dir_name(&path);
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if path.is_dir() {
            collect_blobs(store_path, &path, &child, out)?;
        } else {
            let bytes = fs::read(&path).map_err(|e| read_error(&path, e))?;
            out.push((child, write_blob(store_path, &bytes)?));
        }
    }
    Ok(())
}

fn tree_of_trees(store_path: &Path, trees: &[(String, String)]) -> Result<String, StoreError> {
    let entries: Vec<TreeInput<'_>> = trees
        .iter()
        .map(|(name, sha)| TreeInput {
            mode: "040000",
            sha,
            name,
        })
        .collect();
    mktree(store_path, &entries)
}

/// The directories directly under `dir`, sorted by name. Empty when
/// `dir` does not exist. Files are not listed.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(read_error(dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| read_error(dir, e))?.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// A non-directory entry under `dir` is an error: the `tasks/` and
/// `scanners/` levels hold directories only.
fn require_dirs_only(dir: &Path) -> Result<(), StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(read_error(dir, e)),
    };
    for entry in entries {
        let path = entry.map_err(|e| read_error(dir, e))?.path();
        if !path.is_dir() {
            return Err(StoreError::InvalidPath {
                path: path.display().to_string(),
                reason: "expected a directory".to_string(),
            });
        }
    }
    Ok(())
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .expect("subdirs yields paths with a final component")
        .to_string_lossy()
        .into_owned()
}

fn read_error(path: &Path, e: io::Error) -> StoreError {
    StoreError::Write {
        path: path.to_path_buf(),
        source: e,
    }
}

/// [`ScanFiles`] over a directory: a scan directory's `scan/` tree.
pub struct DirFiles {
    root: PathBuf,
}

impl DirFiles {
    pub fn new(root: &Path) -> Self {
        DirFiles {
            root: root.to_path_buf(),
        }
    }

    fn join(&self, path: &str) -> PathBuf {
        if path.is_empty() {
            self.root.clone()
        } else {
            self.root.join(path)
        }
    }
}

impl ScanFiles for DirFiles {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let full = self.join(path);
        match fs::read(&full) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(read_error(&full, e)),
        }
    }

    fn list_dirs(&self, path: &str) -> Result<Vec<String>, StoreError> {
        Ok(subdirs(&self.join(path))?
            .iter()
            .map(|p| dir_name(p))
            .collect())
    }

    fn list_files(&self, path: &str) -> Result<Vec<String>, StoreError> {
        let dir = self.join(path);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(read_error(&dir, e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| read_error(&dir, e))?.path();
            if path.is_file() {
                names.push(dir_name(&path));
            }
        }
        names.sort();
        Ok(names)
    }

    fn layout(&self) -> Result<u32, StoreError> {
        Ok(CURRENT_LAYOUT)
    }
}

/// [`ScanFiles`] over a commit's tree.
struct CommitFiles<'a> {
    store: &'a Store,
    commit: &'a str,
}

impl ScanFiles for CommitFiles<'_> {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let name = format!("{}:{path}", self.commit);
        match self.store.object_contents(&name)? {
            Some((info, bytes)) if info.kind == "blob" => Ok(Some(bytes)),
            Some((info, _)) => Err(StoreError::Parse(format!(
                "{name} is a {}, not a blob",
                info.kind
            ))),
            None => Ok(None),
        }
    }

    fn list_dirs(&self, path: &str) -> Result<Vec<String>, StoreError> {
        self.list_kind(path, EntryKind::Tree)
    }

    fn list_files(&self, path: &str) -> Result<Vec<String>, StoreError> {
        self.list_kind(path, EntryKind::Blob)
    }

    fn layout(&self) -> Result<u32, StoreError> {
        let header = self.store.read_header(self.commit)?;
        header.version.parse().map_err(|e| {
            StoreError::Parse(format!(
                "object {}: type version {:?}: {e}",
                header.id, header.version
            ))
        })
    }
}

impl CommitFiles<'_> {
    fn list_kind(&self, path: &str, kind: EntryKind) -> Result<Vec<String>, StoreError> {
        let name = format!("{}:{path}", self.commit);
        if self.store.object_info(&name)?.is_none() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = self
            .store
            .read_tree(&name)?
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.name)
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::slice;

    use gage_session::{
        ContentSink, ContentSource, Driver, DriverError, NativeSession, SessionAttrs, Source,
        SourceOptions, StoredSession, WrittenContent,
    };

    use super::*;
    use crate::test_support::open_store;
    use crate::{DatasetStore, SessionStore};
    use gage_core::uuid::new_uuid;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A scan directory `root/<id>/` whose `scan/` tree has one
    /// completed and one failed task. The attrs carry the `skipped`
    /// count an earlier runtime wrote; a reader ignores it.
    fn new_scan_dir(root: &Path, id: &str) -> PathBuf {
        let scan_dir = root.join(id);
        let scan = scan_dir.join("scan");
        write(
            &scan.join("attrs.json"),
            r#"{"runtime":"gage git-abc123","started":1000,"stopped":1500,"canceled":false,"tasks":{"total":2,"completed":1,"failed":1,"skipped":0}}"#,
        );
        write(
            &scan.join("scanners/local/hello/sourcecode.d/hello.rn"),
            "pub fn greet() {}\n",
        );
        write(
            &scan.join("scanners/local/hello/sourcecode.d/..%2Fshared%2Fdoc.md"),
            "shared\n",
        );
        write(
            &scan.join("tasks/local/hello/greet/attrs.json"),
            r#"{"status":"completed","started":1000,"stopped":1200}"#,
        );
        write(
            &scan.join("tasks/local/hello/fail/attrs.json"),
            r#"{"status":"failed","started":1200,"stopped":1500}"#,
        );
        write(&scan.join("logs/err"), "task hello:fail failed\nboom\n");
        write(
            &scan.join("logs/out"),
            "hello, world\nscan SCAN1 completed: 2 tasks\n",
        );
        write(
            &scan.join("logs/records"),
            "2026-09-24T15:04:05.000Z INFO gage_scan: scan started\n2026-09-24T15:04:05.123Z INFO hello:greet: started\n",
        );
        scan_dir
    }

    #[test]
    fn create_writes_the_scan_dir_layout_and_get_reads_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();

        let paths: Vec<String> = store
            .ls(&commit)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            paths,
            [
                "attrs.json",
                "created",
                "id",
                "logs",
                "logs/err",
                "logs/out",
                "logs/records",
                "modified",
                "scanners",
                "scanners/local",
                "scanners/local/hello",
                "scanners/local/hello/sourcecode.d",
                "scanners/local/hello/sourcecode.d/..%2Fshared%2Fdoc.md",
                "scanners/local/hello/sourcecode.d/hello.rn",
                "tasks",
                "tasks/local",
                "tasks/local/hello",
                "tasks/local/hello/fail",
                "tasks/local/hello/fail/attrs.json",
                "tasks/local/hello/greet",
                "tasks/local/hello/greet/attrs.json",
                "type",
            ]
        );
        let header = store.read_header(&commit).unwrap();
        assert_eq!(header.object_type, OBJECT_TYPE);
        assert_eq!(header.version, OBJECT_VERSION);

        let record = scans.get(&id).unwrap();
        assert_eq!(record.id, id);
        assert_eq!(record.commit_sha, commit);
        assert_eq!(
            record.content,
            ScanContent::from_files(&DirFiles::new(&scan_dir.join("scan"))).unwrap()
        );
        assert_eq!(
            record.content.attrs,
            ScanAttrs {
                runtime: "gage git-abc123".into(),
                started: 1000,
                stopped: 1500,
                canceled: false,
                tasks: TaskCounts {
                    total: 2,
                    completed: 1,
                    failed: 1,
                },
            }
        );
        assert_eq!(
            record.content.scanners,
            BTreeMap::from([(
                "local/hello".to_string(),
                vec!["..%2Fshared%2Fdoc.md".to_string(), "hello.rn".to_string()]
            )])
        );
        assert_eq!(
            scans
                .source_file(&commit, "local/hello", "hello.rn")
                .unwrap(),
            Some(b"pub fn greet() {}\n".to_vec())
        );
        assert_eq!(
            scans
                .source_file(&commit, "local/hello", "nope.rn")
                .unwrap(),
            None
        );
        assert_eq!(record.content.logs, ["err", "out", "records"]);
        assert_eq!(
            scans.scan_log(&commit, "out").unwrap(),
            Some(b"hello, world\nscan SCAN1 completed: 2 tasks\n".to_vec())
        );
        assert_eq!(
            scans.scan_log(&commit, "err").unwrap(),
            Some(b"task hello:fail failed\nboom\n".to_vec())
        );
        let failed = &record.content.tasks[0];
        assert_eq!(
            (failed.scanner.as_str(), failed.task.as_str()),
            ("local/hello", "fail")
        );
        assert_eq!(failed.attrs.status, TaskStatus::Failed);
        let greet = &record.content.tasks[1];
        assert_eq!(greet.task, "greet");
        assert_eq!(greet.attrs.status, TaskStatus::Completed);
        assert_eq!(scans.at_commit(&commit).unwrap(), record);
    }

    struct FakeSession {
        id: String,
        source: String,
        content: String,
        attrs: FakeAttrs,
    }

    struct FakeAttrs;

    impl SessionAttrs for FakeAttrs {
        fn native_mtime(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH
        }
        fn native_size(&self) -> u64 {
            0
        }
        fn is_empty(&self) -> Option<bool> {
            Some(false)
        }
        fn project_name(&self) -> Option<&str> {
            None
        }
        fn title(&self) -> Option<&str> {
            None
        }
        fn model(&self) -> Option<&str> {
            None
        }
        fn message_count(&self) -> Option<u64> {
            None
        }
        fn line_count(&self) -> Option<u64> {
            Some(1)
        }
    }

    impl NativeSession for FakeSession {
        fn id(&self) -> &str {
            &self.id
        }
        fn session_type(&self) -> &str {
            "fake"
        }
        fn source(&self) -> &str {
            &self.source
        }
        fn attrs(&self) -> &dyn SessionAttrs {
            &self.attrs
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct FakeDriver;

    impl Driver for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn version(&self) -> &'static str {
            "0.1"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["fake"]
        }
        fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("open_source not used".into()))
        }
        fn split_session_url(&self, _session_url: &str) -> Result<(String, String), DriverError> {
            Err(DriverError::Other("split_session_url not used".into()))
        }
        fn write_native(
            &self,
            session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            let fake = session
                .as_any()
                .downcast_ref::<FakeSession>()
                .ok_or_else(|| DriverError::Other("not a FakeSession".into()))?;
            let mut w = sink.create("session.jsonl").map_err(DriverError::Io)?;
            w.write_all(fake.content.as_bytes())
                .map_err(DriverError::Io)?;
            Ok(WrittenContent::new(
                "fake-lines 1".to_string(),
                1,
                Box::new(FakeAttrs),
            ))
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("read_stored not used".into()))
        }
    }

    /// A task's agent record links the agent session, carries the
    /// agent's attrs, stderr, and result, and the session commit is
    /// a parent of the scan commit. The session's `native_source` is
    /// the value the runtime supplied.
    #[test]
    fn create_writes_a_task_agent_record_and_links_its_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut native = FakeSession {
            id: "agent-native-1".into(),
            source: "fake:agent-native-1".into(),
            content: "{}\n".into(),
            attrs: FakeAttrs,
        };
        let added = SessionStore::from(&store)
            .with_origin("session+task:SCANA/hello:greet/agent-native-1".into())
            .add(&FakeDriver, &mut native)
            .unwrap();
        let record = SessionStore::from(&store).get(&added.id).unwrap();
        assert_eq!(
            record.attrs.origin,
            "session+task:SCANA/hello:greet/agent-native-1"
        );

        let id = new_uuid();

        let scan_dir = new_scan_dir(tmp.path(), &id);
        let agent_dir = scan_dir.join(format!("scan/tasks/local/hello/greet/agents/{}", added.id));
        write(&agent_dir.join("attrs.json"), r#"{"exit_code":0}"#);
        write(&agent_dir.join("stderr"), "warn: x\n");
        write(
            &agent_dir.join("result"),
            r#"{"type":"result","result":"ok"}"#,
        );
        write(
            &scan_dir.join("scan/tasks/local/hello/greet/agents.link"),
            &format!("{}\n", added.commit_sha),
        );
        // An agent still running when the scan ends: its record is
        // the native id alone, read live and left out of the store
        write(
            &scan_dir.join("scan/tasks/local/hello/greet/agents/zzz-running/native_id"),
            "native-2\n",
        );
        write(
            &scan_dir.join("scan/tasks/local/hello/greet/agents/zzz-running/driver"),
            "fake\n",
        );
        let ended = TaskAgent {
            id: added.id.clone(),
            // An earlier runtime's record: every field after the
            // exit code defaults
            record: AgentRecord::Ended(AgentAttrs {
                exit_code: 0,
                is_error: false,
                stop_reason: String::new(),
                turns: 0,
                duration_ms: 0,
                cost_usd: None,
            }),
        };
        let live = read_tasks(&DirFiles::new(&scan_dir.join("scan"))).unwrap();
        assert_eq!(
            live[1].agents,
            [
                ended.clone(),
                TaskAgent {
                    id: "zzz-running".into(),
                    record: AgentRecord::Running {
                        native_id: "native-2".into(),
                        driver: Some("fake".into()),
                    },
                }
            ]
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();

        let greet = &scans.get(&id).unwrap().content.tasks[1];
        assert_eq!(greet.task, "greet");
        assert_eq!(greet.agent_sessions, slice::from_ref(&added.commit_sha));
        assert_eq!(greet.agents, [ended]);
        assert_eq!(
            scans
                .agent_file(&commit, "local/hello", "greet", &added.id, "stderr")
                .unwrap(),
            Some(b"warn: x\n".to_vec())
        );
        assert_eq!(
            scans
                .agent_file(&commit, "local/hello", "greet", &added.id, "result")
                .unwrap(),
            Some(br#"{"type":"result","result":"ok"}"#.to_vec())
        );
        assert!(
            store
                .read_commit(&commit)
                .unwrap()
                .parents
                .contains(&added.commit_sha),
            "the agent session commit is a parent of the scan commit"
        );

        // An agent directory holding anything but the fixed files is
        // rejected
        write(&agent_dir.join("notes.txt"), "x");
        let err = ScanStore::from(&store)
            .create_from_dir(&scan_dir)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidPath { .. }), "{err:?}");
    }

    #[test]
    fn create_without_tasks_writes_no_tasks_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = tmp.path().join(&id);
        write(
            &scan_dir.join("scan/attrs.json"),
            r#"{"runtime":"gage 0.2.0","started":1,"stopped":2,"canceled":false,"tasks":{"total":0,"completed":0,"failed":0}}"#,
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();
        let paths: Vec<String> = store
            .ls(&commit)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(paths, ["attrs.json", "created", "id", "modified", "type"]);
        assert!(scans.get(&id).unwrap().content.tasks.is_empty());
    }

    #[test]
    fn query_lists_scans_newest_first_with_count_ignoring_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let ids: Vec<String> = (0..3).map(|_| new_uuid()).collect();
        let scans = ScanStore::from(&store);
        for id in &ids {
            scans
                .create_from_dir(&new_scan_dir(tmp.path(), id))
                .unwrap();
            // Distinct `created` stamps so the order is deterministic
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(scans.query().limit(2).count().unwrap(), 3);
        let listed: Vec<String> = scans
            .query()
            .limit(2)
            .iter()
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(listed, [ids[2].clone(), ids[1].clone()]);
        let all: Vec<String> = scans.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(all, [ids[2].clone(), ids[1].clone(), ids[0].clone()]);
        assert_eq!(
            scans
                .query()
                .order(Order::CreatedAsc)
                .iter()
                .unwrap()
                .count(),
            3
        );
    }

    #[test]
    fn create_rejects_an_in_flight_task_status() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(
            &scan_dir.join("scan/tasks/local/hello/greet/attrs.json"),
            r#"{"status":"started","started":1000}"#,
        );
        let err = ScanStore::from(&store)
            .create_from_dir(&scan_dir)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("local/hello:greet has status started"),
            "{err}"
        );
        assert!(store.list_object_refs().unwrap().is_empty());
    }

    #[test]
    fn create_rejects_a_stray_file_in_a_scanner_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(&scan_dir.join("scan/scanners/local/hello/notes.txt"), "x");
        let err = ScanStore::from(&store)
            .create_from_dir(&scan_dir)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidPath { .. }), "{err}");
    }

    #[test]
    fn create_rejects_an_unknown_log_name() {
        for path in ["tasks/local/hello/greet/logs/out", "logs/trace"] {
            let tmp = tempfile::tempdir().unwrap();
            let (store, _fsck) = open_store(tmp.path());
            let id = new_uuid();
            let scan_dir = new_scan_dir(tmp.path(), &id);
            write(&scan_dir.join("scan").join(path), "x");
            let err = ScanStore::from(&store)
                .create_from_dir(&scan_dir)
                .unwrap_err();
            assert!(
                matches!(err, StoreError::InvalidPath { .. }),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn create_rejects_an_unexpected_task_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(
            &scan_dir.join("scan/tasks/local/hello/greet/notes.txt"),
            "x",
        );
        let err = ScanStore::from(&store)
            .create_from_dir(&scan_dir)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidPath { .. }), "{err}");
    }

    #[test]
    fn create_links_the_dataset_commit_as_a_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset_id = datasets.create().unwrap();
        let dataset_sha = datasets.get(&dataset_id).unwrap().commit_sha;
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(
            &scan_dir.join("scan/dataset.link"),
            &format!("{dataset_sha}\n"),
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();

        assert_eq!(
            store.read_commit(&commit).unwrap().parents,
            slice::from_ref(&dataset_sha)
        );
        let record = scans.get(&id).unwrap();
        assert_eq!(record.content.dataset, Some(dataset_sha));
        assert_eq!(
            record.content,
            ScanContent::from_files(&DirFiles::new(&scan_dir.join("scan"))).unwrap()
        );
    }

    #[test]
    fn the_partial_readers_return_the_same_parts_as_the_full_record() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset_id = datasets.create().unwrap();
        let dataset_sha = datasets.get(&dataset_id).unwrap().commit_sha;
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(
            &scan_dir.join("scan/dataset.link"),
            &format!("{dataset_sha}\n"),
        );
        write(
            &scan_dir.join("scan/plan.json"),
            r#"{"tasks":[{"task":"hello:greet","selected":"explicit"}]}"#,
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();
        let content = scans.get(&id).unwrap().content;
        assert!(content.dataset.is_some());
        assert!(content.plan.is_some());
        assert_eq!(content.tasks.len(), 2);

        assert_eq!(
            scans.summary_at_commit(&commit).unwrap(),
            ScanSummary {
                attrs: content.attrs,
                dataset: content.dataset,
            }
        );
        assert_eq!(scans.tasks_at_commit(&commit).unwrap(), content.tasks);
        assert_eq!(scans.plan_at_commit(&commit).unwrap(), content.plan);
    }

    #[test]
    fn the_partial_readers_reject_a_commit_that_is_not_a_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let dataset_id = datasets.create().unwrap();
        let dataset_sha = datasets.get(&dataset_id).unwrap().commit_sha;
        let scans = ScanStore::from(&store);

        let wrong_type = |err: StoreError| {
            assert!(
                matches!(&err, StoreError::WrongType { expected, actual, .. }
                    if expected == OBJECT_TYPE && actual == DATASET_TYPE),
                "{err}"
            );
        };
        wrong_type(scans.summary_at_commit(&dataset_sha).unwrap_err());
        wrong_type(scans.tasks_at_commit(&dataset_sha).unwrap_err());
        wrong_type(scans.plan_at_commit(&dataset_sha).unwrap_err());
    }

    #[test]
    fn create_without_a_dataset_link_has_no_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();
        assert!(store.read_commit(&commit).unwrap().parents.is_empty());
        assert_eq!(scans.get(&id).unwrap().content.dataset, None);
    }

    #[test]
    fn create_rejects_a_dataset_link_to_a_non_dataset() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let other = ScanStore::from(&store)
            .create_from_dir(&new_scan_dir(tmp.path(), &new_uuid()))
            .unwrap();
        let scan_dir = new_scan_dir(tmp.path(), &new_uuid());
        write(&scan_dir.join("scan/dataset.link"), &format!("{other}\n"));
        let err = ScanStore::from(&store)
            .create_from_dir(&scan_dir)
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::WrongType { expected, actual, .. }
                if expected == DATASET_TYPE && actual == OBJECT_TYPE),
            "{err}"
        );
    }

    #[test]
    fn create_rejects_a_malformed_dataset_link() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let datasets = DatasetStore::from(&store);
        let sha = datasets
            .get(&datasets.create().unwrap())
            .unwrap()
            .commit_sha;
        for (content, what) in [
            (format!("{sha}\n{sha}\n"), "expected one SHA, found 2"),
            ("\n".to_string(), "expected one SHA, found 0"),
            ("not-a-sha\n".to_string(), "is not a commit SHA"),
        ] {
            let id = new_uuid();
            let scan_dir = new_scan_dir(tmp.path(), &id);
            write(&scan_dir.join("scan/dataset.link"), &content);
            let err = ScanStore::from(&store)
                .create_from_dir(&scan_dir)
                .unwrap_err();
            assert!(
                matches!(&err, StoreError::Parse(m) if m.contains(what)),
                "{content:?}: {err}"
            );
        }
    }

    #[test]
    fn create_links_every_note_in_notes_link() {
        use crate::{NoteInput, NoteStore, NoteValue};
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let mut shas = Vec::new();
        for name in ["a", "b"] {
            let id = notes
                .create(NoteInput {
                    name,
                    value: NoteValue::Text(name.into()),
                    author: "task:s:t",
                    target: None,
                    metadata: None,
                    work_id: None,
                })
                .unwrap();
            shas.push(
                store
                    .rev_parse(&crate::object::object_ref(&id))
                    .unwrap()
                    .unwrap(),
            );
        }
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(
            &scan_dir.join("scan/notes.link"),
            &format!("{}\n{}\n", shas[0], shas[1]),
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();
        assert_eq!(store.read_commit(&commit).unwrap().parents, shas);
        assert_eq!(scans.get(&id).unwrap().content.notes, shas);
    }

    #[test]
    fn create_links_every_issue_in_issues_link_and_rejects_a_non_issue() {
        use crate::{IssueInput, IssueStatus, IssueStore};
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = IssueStore::from(&store)
            .create(IssueInput {
                name: "n",
                title: "t",
                description: None,
                author: "task:s:t",
                status: IssueStatus::Pending,
                evidence: &[],
                key: None,
            })
            .unwrap();
        let sha = store
            .rev_parse(&crate::object::object_ref(&id))
            .unwrap()
            .unwrap();
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        write(&scan_dir.join("scan/issues.link"), &format!("{sha}\n"));
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();
        assert_eq!(
            store.read_commit(&commit).unwrap().parents,
            slice::from_ref(&sha)
        );
        assert_eq!(scans.get(&id).unwrap().content.issues, [sha]);

        let other_dir = new_scan_dir(tmp.path(), &new_uuid());
        write(&other_dir.join("scan/issues.link"), &format!("{commit}\n"));
        let err = scans.create_from_dir(&other_dir).unwrap_err();
        assert!(
            matches!(&err, StoreError::WrongType { expected, .. } if expected == "gage::issue"),
            "{err}"
        );
    }

    #[test]
    fn create_rejects_a_notes_link_to_a_non_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let other = ScanStore::from(&store)
            .create_from_dir(&new_scan_dir(tmp.path(), &new_uuid()))
            .unwrap();
        let scan_dir = new_scan_dir(tmp.path(), &new_uuid());
        write(&scan_dir.join("scan/notes.link"), &format!("{other}\n"));
        let err = ScanStore::from(&store)
            .create_from_dir(&scan_dir)
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::WrongType { expected, .. } if expected == NOTE_TYPE),
            "{err}"
        );
    }

    #[test]
    fn create_imports_watermarks_and_decodes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        let session_sha = "0123456789abcdef0123456789abcdef01234567";
        let note_sha = "89abcdef0123456789abcdef0123456789abcdef";
        let digest = "0123456789abcdef";
        write(
            &scan_dir.join("scan/watermarks/SESSION1/s:t:1"),
            &format!("{session_sha} 120\n"),
        );
        write(
            &scan_dir.join("scan/watermarks/NOTE1/s:t:1"),
            &format!("{note_sha} 1\n"),
        );
        write(
            &scan_dir.join("scan/watermarks/ATTACHMENT1/s:t:1"),
            &format!("{digest} 1\n"),
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create_from_dir(&scan_dir).unwrap();
        let record = scans.get(&id).unwrap();
        assert_eq!(
            record.content.watermarks,
            [
                Watermark {
                    oid: "ATTACHMENT1".into(),
                    work_id: "s:t:1".into(),
                    version: digest.into(),
                    mark: 1,
                },
                Watermark {
                    oid: "NOTE1".into(),
                    work_id: "s:t:1".into(),
                    version: note_sha.into(),
                    mark: 1,
                },
                Watermark {
                    oid: "SESSION1".into(),
                    work_id: "s:t:1".into(),
                    version: session_sha.into(),
                    mark: 120,
                },
            ]
        );
        assert_eq!(
            record.content,
            ScanContent::from_files(&DirFiles::new(&scan_dir.join("scan"))).unwrap()
        );
        assert!(
            store
                .ls(&commit)
                .unwrap()
                .iter()
                .any(|e| e.name == "watermarks/SESSION1/s:t:1")
        );
    }

    #[test]
    fn create_rejects_a_watermark_without_a_version_and_a_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp.path(), &id);
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for content in [
            "1234 7\n",
            "0123456789ABCDEF 7\n",
            &format!("{sha}\n"),
            &format!("{sha} x\n"),
        ] {
            write(&scan_dir.join("scan/watermarks/SESSION1/s:t:1"), content);
            let err = ScanStore::from(&store)
                .create_from_dir(&scan_dir)
                .unwrap_err();
            assert!(
                matches!(&err, StoreError::Parse(m) if m.contains("is not <version> <mark>")),
                "{content:?}: {err}"
            );
        }
    }

    /// A stored scan under `id` linking two written notes, one carried
    /// note, and one issue. Returns the scan commit and the ids of the
    /// written notes, the carried note, and the issue.
    fn scan_with_links(
        tmp: &Path,
        store: &Store,
        id: &str,
    ) -> (String, [String; 2], String, String) {
        use crate::{IssueInput, IssueStatus, IssueStore, NoteInput, NoteStore, NoteValue};
        let notes = NoteStore::from(store);
        let note = |name: &str| {
            notes
                .create(NoteInput {
                    name,
                    value: NoteValue::Text(name.into()),
                    author: "task:s:t",
                    target: None,
                    metadata: None,
                    work_id: None,
                })
                .unwrap()
        };
        let sha = |id: &str| {
            store
                .rev_parse(&crate::object::object_ref(id))
                .unwrap()
                .unwrap()
        };
        let written = [note("a"), note("b")];
        let carried = note("c");
        let issue = IssueStore::from(store)
            .create(IssueInput {
                name: "n",
                title: "t",
                description: None,
                author: "task:s:t",
                status: IssueStatus::Pending,
                evidence: &[],
                key: None,
            })
            .unwrap();
        let scan_dir = new_scan_dir(tmp, id);
        write(
            &scan_dir.join("scan/notes.link"),
            &format!("{}\n{}\n", sha(&written[0]), sha(&written[1])),
        );
        write(
            &scan_dir.join("scan/notes_carried.link"),
            &format!("{}\n", sha(&carried)),
        );
        write(
            &scan_dir.join("scan/issues.link"),
            &format!("{}\n", sha(&issue)),
        );
        let commit = ScanStore::from(store).create_from_dir(&scan_dir).unwrap();
        (commit, written, carried, issue)
    }

    #[test]
    fn delete_tombstones_the_scan_and_leaves_its_notes_and_issues() {
        use crate::{IssueStore, NoteStore};
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let (_, written, carried, issue) = scan_with_links(tmp.path(), &store, &id);
        let scans = ScanStore::from(&store);

        assert_eq!(scans.delete(&id).unwrap(), id);

        assert!(matches!(
            scans.get(&id).unwrap_err(),
            StoreError::ObjectDeleted(found) if found == id
        ));
        assert_eq!(scans.query().count().unwrap(), 0);
        let notes = NoteStore::from(&store);
        for id in written.iter().chain([&carried]) {
            notes.get(id).unwrap();
        }
        IssueStore::from(&store).get(&issue).unwrap();
        assert!(matches!(
            scans.delete(&id).unwrap_err(),
            StoreError::ObjectDeleted(found) if found == id
        ));
    }

    #[test]
    fn delete_cascade_tombstones_written_notes_and_issues_and_keeps_carried() {
        use crate::{IssueStore, NoteStore};
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = new_uuid();
        let (_, written, carried, issue) = scan_with_links(tmp.path(), &store, &id);
        let scans = ScanStore::from(&store);
        let notes = NoteStore::from(&store);
        let issues = IssueStore::from(&store);
        // A note deleted after the scan wrote it is skipped, not an error
        notes.delete(&written[1]).unwrap();

        let deleted = scans.delete_cascade(&id).unwrap();

        assert_eq!(
            deleted,
            ScanDeleted {
                scan: id.clone(),
                notes: vec![written[0].clone()],
                issues: vec![issue.clone()],
                notes_kept: Vec::new(),
                issues_kept: Vec::new(),
            }
        );
        assert!(matches!(
            scans.get(&id).unwrap_err(),
            StoreError::ObjectDeleted(found) if found == id
        ));
        for id in &written {
            assert!(matches!(
                notes.get(id).unwrap_err(),
                StoreError::ObjectDeleted(x) if &x == id
            ));
        }
        notes.get(&carried).unwrap();
        assert!(matches!(
            issues.get(&issue).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == issue
        ));
    }

    fn new_note(store: &Store, name: &str) -> String {
        use crate::{NoteInput, NoteStore, NoteValue};
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text(name.into()),
                author: "task:s:t",
                target: None,
                metadata: None,
                work_id: None,
            })
            .unwrap()
    }

    fn new_issue(store: &Store, evidence: &[String], key: Option<&str>) -> String {
        use crate::{IssueInput, IssueStatus, IssueStore};
        IssueStore::from(store)
            .create(IssueInput {
                name: "n",
                title: "t",
                description: None,
                author: "task:s:t",
                status: IssueStatus::Pending,
                evidence,
                key,
            })
            .unwrap()
    }

    /// Write the live issue `id` again, as a later scan does under the
    /// issue's key. Returns the new commit.
    fn rewrite_issue(tmp: &Path, store: &Store, id: &str, evidence: &[String]) -> String {
        use crate::{IssueInput, IssueStatus, IssueStore};
        let issues = IssueStore::from(store);
        let prev = issues.get(id).unwrap();
        let dir = tmp.join(new_uuid()).join(id);
        let input = IssueInput {
            name: "n",
            title: "t again",
            description: None,
            author: "task:s:t",
            status: IssueStatus::Pending,
            evidence,
            key: prev.key.as_deref(),
        };
        issues
            .write_replace_to_dir(&dir, &input, &new_uuid(), &prev)
            .unwrap();
        issues.apply_from_dir(&dir).unwrap().1
    }

    fn tip(store: &Store, id: &str) -> String {
        store
            .rev_parse(&crate::object::object_ref(id))
            .unwrap()
            .unwrap()
    }

    /// A stored scan whose `scan/` tree holds the given link files,
    /// each a name and the commits it lists. Returns the scan id.
    fn scan_linking(tmp: &Path, store: &Store, links: &[(&str, &[String])]) -> String {
        let id = new_uuid();
        let scan_dir = new_scan_dir(tmp, &id);
        for (name, shas) in links {
            let content: String = shas.iter().map(|sha| format!("{sha}\n")).collect();
            write(&scan_dir.join("scan").join(name), &content);
        }
        ScanStore::from(store).create_from_dir(&scan_dir).unwrap();
        id
    }

    #[test]
    fn delete_cascade_keeps_an_issue_an_earlier_scan_also_wrote() {
        use crate::IssueStore;
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let issues = IssueStore::from(&store);
        let scans = ScanStore::from(&store);
        // Scan A writes the keyed issue; scan B writes it again, which
        // is a later commit of the same object
        let issue = new_issue(&store, &[], Some("k"));
        let first = tip(&store, &issue);
        let a = scan_linking(
            tmp.path(),
            &store,
            &[("issues.link", slice::from_ref(&first))],
        );
        let second = rewrite_issue(tmp.path(), &store, &issue, &[]);
        assert_ne!(first, second);
        let b = scan_linking(
            tmp.path(),
            &store,
            &[("issues.link", slice::from_ref(&second))],
        );

        let deleted = scans.delete_cascade(&b).unwrap();

        assert_eq!(
            deleted,
            ScanDeleted {
                scan: b,
                notes: Vec::new(),
                issues: Vec::new(),
                notes_kept: Vec::new(),
                issues_kept: vec![issue.clone()],
            }
        );
        assert_eq!(tip(&store, &issue), second, "the kept issue is unchanged");
        assert_eq!(scans.get(&a).unwrap().content.issues, vec![first]);

        // Scan A is the last to link the issue, at an earlier commit
        let deleted = scans.delete_cascade(&a).unwrap();

        assert_eq!(deleted.issues, vec![issue.clone()]);
        assert_eq!(deleted.issues_kept, Vec::<String>::new());
        assert!(matches!(
            issues.get(&issue).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == issue
        ));
    }

    #[test]
    fn delete_cascade_keeps_a_note_a_later_scan_carried() {
        use crate::NoteStore;
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let scans = ScanStore::from(&store);
        // Scan A writes a note on a session's first lines. The session
        // grows, and scan B carries the note and writes one of its own
        let early = new_note(&store, "early");
        let early_sha = tip(&store, &early);
        let a = scan_linking(
            tmp.path(),
            &store,
            &[("notes.link", slice::from_ref(&early_sha))],
        );
        let late = new_note(&store, "late");
        let late_sha = tip(&store, &late);
        let b = scan_linking(
            tmp.path(),
            &store,
            &[
                ("notes.link", slice::from_ref(&late_sha)),
                ("notes_carried.link", slice::from_ref(&early_sha)),
            ],
        );

        let deleted = scans.delete_cascade(&a).unwrap();

        assert_eq!(
            deleted,
            ScanDeleted {
                scan: a,
                notes: Vec::new(),
                issues: Vec::new(),
                notes_kept: vec![early.clone()],
                issues_kept: Vec::new(),
            }
        );
        assert_eq!(tip(&store, &early), early_sha, "the kept note is unchanged");
        // Every note the surviving scan lists is live
        let content = scans.get(&b).unwrap().content;
        for sha in content.notes.iter().chain(&content.notes_carried) {
            notes.get(&store.read_header(sha).unwrap().id).unwrap();
        }
    }

    #[test]
    fn delete_cascade_keeps_a_note_another_issue_cites_and_not_one_its_own_issue_cites() {
        use crate::{IssueStore, NoteStore};
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let issues = IssueStore::from(&store);
        let own = new_note(&store, "own");
        let cited = new_note(&store, "cited");
        let own_issue = new_issue(&store, slice::from_ref(&own), None);
        let other_issue = new_issue(&store, slice::from_ref(&cited), None);
        let scan = scan_linking(
            tmp.path(),
            &store,
            &[
                ("notes.link", &[tip(&store, &own), tip(&store, &cited)]),
                ("issues.link", &[tip(&store, &own_issue)]),
            ],
        );

        let deleted = ScanStore::from(&store).delete_cascade(&scan).unwrap();

        assert_eq!(
            deleted,
            ScanDeleted {
                scan,
                notes: vec![own.clone()],
                issues: vec![own_issue.clone()],
                notes_kept: vec![cited.clone()],
                issues_kept: Vec::new(),
            }
        );
        assert!(matches!(
            notes.get(&own).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == own
        ));
        notes.get(&cited).unwrap();
        issues.get(&other_issue).unwrap();
    }

    #[test]
    fn delete_cascade_ignores_a_dead_scan_an_earlier_link_and_a_note_target() {
        use crate::{NoteInput, NoteStore, NoteValue};
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);
        let issues = IssueStore::from(&store);
        let scans = ScanStore::from(&store);
        let note = new_note(&store, "a");
        let note_sha = tip(&store, &note);
        // An issue cited the note once and no longer does
        let issue = new_issue(&store, slice::from_ref(&note), None);
        rewrite_issue(tmp.path(), &store, &issue, &[]);
        assert_eq!(issues.get(&issue).unwrap().evidence, Vec::<String>::new());
        // A note about the note
        notes
            .create(NoteInput {
                name: "about",
                value: NoteValue::Text("about".into()),
                author: "task:s:t",
                target: Some(&format!("note:{note}")),
                metadata: None,
                work_id: None,
            })
            .unwrap();
        // A scan that linked the note and is deleted
        let dead = scan_linking(
            tmp.path(),
            &store,
            &[("notes_carried.link", slice::from_ref(&note_sha))],
        );
        scans.delete(&dead).unwrap();
        let scan = scan_linking(
            tmp.path(),
            &store,
            &[("notes.link", slice::from_ref(&note_sha))],
        );

        let deleted = scans.delete_cascade(&scan).unwrap();

        assert_eq!(deleted.notes, vec![note.clone()]);
        assert_eq!(deleted.notes_kept, Vec::<String>::new());
        assert!(matches!(
            notes.get(&note).unwrap_err(),
            StoreError::ObjectDeleted(x) if x == note
        ));
    }
}
