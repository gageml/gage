//! Scan objects: `gage::scan 1`, reached through [`ScanStore`].
//!
//! Content is `attrs.json` and, under `tasks/<scanner>/<task>/`, each
//! task's `attrs.json` and, for a failed task, `error.txt`. The layout
//! is the same in a scan's staging directory and in the store, so one
//! decoder serves both through [`ScanFiles`]: [`DirFiles`] over a
//! directory and the store's own view over a commit.
//! [`ScanStore::create`] imports a staging `scan/` directory as the
//! object's content. Links, scanner source, agent records, logs, and
//! validation records are not written yet.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git::EntryKind;
use crate::index::{ObjectQuery, Order};
use crate::object::{ObjectTree, require_type};
use crate::writer::{TreeInput, mktree, write_blob};
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::scan";
const OBJECT_VERSION: &str = "1";
pub(crate) const INDEXED_ATTRS: &[&str] = &[];
const TASKS_DIR: &str = "tasks";
const ATTRS_FILE: &str = "attrs.json";
const ERROR_FILE: &str = "error.txt";

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
    pub skipped: usize,
}

/// A task's `status`. `Pending` and `Started` occur only in staging
/// while the scan runs; a stored scan carries the terminal statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Started,
    Completed,
    Failed,
    Skipped,
    Canceled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Started => "started",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Canceled => "canceled",
        }
    }

    /// True for a status a stored scan may carry.
    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskStatus::Pending | TaskStatus::Started)
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
    /// Accumulated working time, excluding agent pool waits. Written
    /// by no writer today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worked_ms: Option<u64>,
}

/// One task of a scan, decoded from `tasks/<scanner>/<task>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTask {
    pub scanner: String,
    pub task: String,
    pub attrs: TaskAttrs,
    /// The failure message from `error.txt`, when present
    pub error: Option<String>,
}

/// A scan's content, decoded from either location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanContent {
    pub attrs: ScanAttrs,
    /// In `tasks/` tree order: by scanner name, then task name
    pub tasks: Vec<ScanTask>,
}

/// A scan read from the store: its content plus the object markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRecord {
    pub id: String,
    pub commit_sha: String,
    pub content: ScanContent,
    /// From the `created` blob: the time the record was applied to
    /// the store, unrelated to the scan's `started`.
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// A read-only view of a scan's files. Paths are `/`-separated and
/// relative to the scan root; the root itself is `""`.
pub trait ScanFiles {
    /// The bytes of the file at `path`, or `None` when there is none.
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;
    /// The names of the directories directly under `path`, sorted.
    /// Empty when `path` does not exist.
    fn list_dirs(&self, path: &str) -> Result<Vec<String>, StoreError>;
}

impl ScanStore<'_> {
    /// Write a scan from its staging `scan/` directory under the given
    /// id. The directory is validated first: every task status must be
    /// terminal, and a task directory may hold only `attrs.json` and
    /// `error.txt`. Returns the commit SHA.
    pub fn create(&self, id: &str, scan_dir: &Path) -> Result<String, StoreError> {
        let content = ScanContent::from_files(&DirFiles::new(scan_dir))?;
        for task in &content.tasks {
            if !task.attrs.status.is_terminal() {
                return Err(StoreError::Parse(format!(
                    "task {}:{} has status {}, which is not terminal",
                    task.scanner,
                    task.task,
                    task.attrs.status.as_str()
                )));
            }
        }
        let mut tree = ObjectTree {
            attrs: Some(
                serde_json::to_value(&content.attrs)
                    .map_err(|e| StoreError::Parse(format!("scan attrs encode: {e}")))?,
            ),
            ..ObjectTree::default()
        };
        if let Some(sha) = import_tasks_tree(self.store.path(), &scan_dir.join(TASKS_DIR))? {
            tree.subtrees.insert(TASKS_DIR.to_string(), sha);
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
    /// task `attrs.json` are required.
    pub fn from_files(files: &dyn ScanFiles) -> Result<ScanContent, StoreError> {
        let attrs = read_json(files, ATTRS_FILE)?;
        let mut tasks = Vec::new();
        for scanner in files.list_dirs(TASKS_DIR)? {
            let scanner_path = format!("{TASKS_DIR}/{scanner}");
            for task in files.list_dirs(&scanner_path)? {
                let task_path = format!("{scanner_path}/{task}");
                let attrs = read_json(files, &format!("{task_path}/{ATTRS_FILE}"))?;
                let error = files
                    .read(&format!("{task_path}/{ERROR_FILE}"))?
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                tasks.push(ScanTask {
                    scanner: scanner.clone(),
                    task,
                    attrs,
                    error,
                });
            }
        }
        Ok(ScanContent { attrs, tasks })
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(
    files: &dyn ScanFiles,
    path: &str,
) -> Result<T, StoreError> {
    let bytes = files
        .read(path)?
        .ok_or_else(|| StoreError::Parse(format!("scan file {path} is missing")))?;
    serde_json::from_slice(&bytes).map_err(|e| StoreError::Parse(format!("scan file {path}: {e}")))
}

/// Build the `tasks/` tree from a staging `tasks/` directory. Returns
/// `None` when the directory is absent or holds no scanner.
fn import_tasks_tree(store_path: &Path, tasks_dir: &Path) -> Result<Option<String>, StoreError> {
    let mut scanner_trees: Vec<(String, String)> = Vec::new();
    for scanner_dir in subdirs(tasks_dir)? {
        let mut task_trees: Vec<(String, String)> = Vec::new();
        for task_dir in subdirs(&scanner_dir)? {
            task_trees.push((
                dir_name(&task_dir),
                import_task_tree(store_path, &task_dir)?,
            ));
        }
        scanner_trees.push((
            dir_name(&scanner_dir),
            tree_of_trees(store_path, &task_trees)?,
        ));
    }
    if scanner_trees.is_empty() {
        return Ok(None);
    }
    Ok(Some(tree_of_trees(store_path, &scanner_trees)?))
}

/// Build one task's tree. Only `attrs.json` and `error.txt` are
/// accepted; anything else in the directory is an error.
fn import_task_tree(store_path: &Path, task_dir: &Path) -> Result<String, StoreError> {
    let mut blobs: BTreeMap<String, String> = BTreeMap::new();
    for entry in fs::read_dir(task_dir).map_err(|e| read_error(task_dir, e))? {
        let entry = entry.map_err(|e| read_error(task_dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !path.is_file() || !(name == ATTRS_FILE || name == ERROR_FILE) {
            return Err(StoreError::InvalidPath {
                path: path.display().to_string(),
                reason: "unexpected entry in a task directory".to_string(),
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
/// `dir` does not exist. A non-directory entry is an error: the
/// `tasks/` levels hold directories only.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(read_error(dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| read_error(dir, e))?.path();
        if !path.is_dir() {
            return Err(StoreError::InvalidPath {
                path: path.display().to_string(),
                reason: "expected a directory".to_string(),
            });
        }
        out.push(path);
    }
    out.sort();
    Ok(out)
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

/// [`ScanFiles`] over a directory: a scan's staging `scan/` directory.
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
        let name = format!("{}:{path}", self.commit);
        if self.store.object_info(&name)?.is_none() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = self
            .store
            .read_tree(&name)?
            .into_iter()
            .filter(|e| e.kind == EntryKind::Tree)
            .map(|e| e.name)
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A staging `scan/` directory with one completed and one failed
    /// task.
    fn staged_scan(dir: &Path) -> PathBuf {
        let scan = dir.join("scan");
        write(
            &scan.join("attrs.json"),
            r#"{"started":1000,"stopped":1500,"canceled":false,"tasks":{"total":2,"completed":1,"failed":1,"skipped":0}}"#,
        );
        write(
            &scan.join("tasks/hello/greet/attrs.json"),
            r#"{"status":"completed","started":1000,"stopped":1200}"#,
        );
        write(
            &scan.join("tasks/hello/fail/attrs.json"),
            r#"{"status":"failed","started":1200,"stopped":1500}"#,
        );
        write(&scan.join("tasks/hello/fail/error.txt"), "boom\nline 2\n");
        scan
    }

    #[test]
    fn create_writes_the_staged_layout_and_get_reads_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let scan_dir = staged_scan(tmp.path());
        let scans = ScanStore::from(&store);
        let commit = scans.create("SCAN1", &scan_dir).unwrap();

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
                "modified",
                "tasks",
                "tasks/hello",
                "tasks/hello/fail",
                "tasks/hello/fail/attrs.json",
                "tasks/hello/fail/error.txt",
                "tasks/hello/greet",
                "tasks/hello/greet/attrs.json",
                "type",
            ]
        );
        let header = store.read_header(&commit).unwrap();
        assert_eq!(header.object_type, OBJECT_TYPE);
        assert_eq!(header.version, OBJECT_VERSION);

        let record = scans.get("SCAN1").unwrap();
        assert_eq!(record.id, "SCAN1");
        assert_eq!(record.commit_sha, commit);
        assert_eq!(
            record.content,
            ScanContent::from_files(&DirFiles::new(&scan_dir)).unwrap()
        );
        assert_eq!(
            record.content.attrs,
            ScanAttrs {
                started: 1000,
                stopped: 1500,
                canceled: false,
                tasks: TaskCounts {
                    total: 2,
                    completed: 1,
                    failed: 1,
                    skipped: 0,
                },
            }
        );
        let failed = &record.content.tasks[0];
        assert_eq!(
            (failed.scanner.as_str(), failed.task.as_str()),
            ("hello", "fail")
        );
        assert_eq!(failed.attrs.status, TaskStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("boom\nline 2\n"));
        let greet = &record.content.tasks[1];
        assert_eq!(greet.task, "greet");
        assert_eq!(greet.error, None);
        assert_eq!(scans.at_commit(&commit).unwrap(), record);
    }

    #[test]
    fn create_without_tasks_writes_no_tasks_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let scan_dir = tmp.path().join("scan");
        write(
            &scan_dir.join("attrs.json"),
            r#"{"started":1,"stopped":2,"canceled":false,"tasks":{"total":0,"completed":0,"failed":0,"skipped":0}}"#,
        );
        let scans = ScanStore::from(&store);
        let commit = scans.create("SCAN2", &scan_dir).unwrap();
        let paths: Vec<String> = store
            .ls(&commit)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(paths, ["attrs.json", "created", "id", "modified", "type"]);
        assert!(scans.get("SCAN2").unwrap().content.tasks.is_empty());
    }

    #[test]
    fn query_lists_scans_newest_first_with_count_ignoring_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let scan_dir = staged_scan(tmp.path());
        let scans = ScanStore::from(&store);
        for id in ["SCANA", "SCANB", "SCANC"] {
            scans.create(id, &scan_dir).unwrap();
            // Distinct `created` stamps so the order is deterministic
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(scans.query().limit(2).count().unwrap(), 3);
        let ids: Vec<String> = scans
            .query()
            .limit(2)
            .iter()
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(ids, ["SCANC", "SCANB"]);
        let all: Vec<String> = scans.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(all, ["SCANC", "SCANB", "SCANA"]);
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
        let scan_dir = staged_scan(tmp.path());
        write(
            &scan_dir.join("tasks/hello/greet/attrs.json"),
            r#"{"status":"started","started":1000}"#,
        );
        let err = ScanStore::from(&store)
            .create("SCAN3", &scan_dir)
            .unwrap_err();
        assert!(
            err.to_string().contains("hello:greet has status started"),
            "{err}"
        );
        assert!(store.list_object_refs().unwrap().is_empty());
    }

    #[test]
    fn create_rejects_an_unexpected_task_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let scan_dir = staged_scan(tmp.path());
        write(&scan_dir.join("tasks/hello/greet/notes.txt"), "x");
        let err = ScanStore::from(&store)
            .create("SCAN4", &scan_dir)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidPath { .. }), "{err}");
    }
}
