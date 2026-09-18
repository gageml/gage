//! Dataset objects: `gage::dataset 1`.
//!
//! Content is a `sessions.link` file listing the commit SHA of each
//! member session in insertion order; the position in that file is the
//! session number used by dataset session commands. Every listed SHA
//! is a commit parent, so pushing a dataset carries its sessions along.
//! Session content lives in [`gage::session`](crate::session) objects;
//! the dataset does not copy it. Tree construction, commit parents,
//! and edits are the generic object model's job; see
//! [`crate::object`].

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use gage_core::uuid::new_uuid;
use gage_session::{ContentAccess, SessionType, SourceSession};

use crate::git::{git_in, run};
use crate::object::{EditOutcome, Object, ObjectTree, read_object_at, require_type, resolve_id_at};
use crate::session::{SessionAddOutcome, SessionOutcome, session_add_at, session_at_commit};
use crate::{StoreError, exists, object, store_path};

const OBJECT_TYPE: &str = "gage::dataset";
const OBJECT_VERSION: &str = "1";
const SESSIONS_LINK: &str = "sessions.link";

/// Create an empty dataset in the default store. Returns the new id.
pub fn dataset_new() -> Result<String, StoreError> {
    dataset_new_at(&store_path())
}

/// Create an empty dataset in the store at `path`. Returns the new id.
pub fn dataset_new_at(path: &Path) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let id = new_uuid();
    object::create(
        path,
        OBJECT_TYPE,
        OBJECT_VERSION,
        &id,
        &ObjectTree::default(),
        "dataset",
    )?;
    Ok(id)
}

/// A dataset summary row for the list view.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetRecord {
    pub id: String,
    pub created_ms: i64,
}

/// List every dataset in the default store, newest first by committer
/// date.
pub fn dataset_list() -> Result<Vec<DatasetRecord>, StoreError> {
    dataset_list_at(&store_path())
}

/// List every dataset in the store at `path`, newest first by
/// committer date. Tombstones and objects of other types are skipped.
pub fn dataset_list_at(path: &Path) -> Result<Vec<DatasetRecord>, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let listing = run(git_in(
        path,
        [
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(objectname)",
            "refs/gage/object/",
        ],
    ))?;
    let mut records = Vec::new();
    for sha in listing.lines() {
        let object = read_object_at(path, sha)?;
        if object.header.object_type != OBJECT_TYPE || object.header.is_tombstone() {
            continue;
        }
        let created_ms = object.header.created_ms.ok_or_else(|| {
            StoreError::Parse(format!("dataset {}: missing created", object.header.id))
        })?;
        records.push(DatasetRecord {
            id: object.header.id,
            created_ms,
        });
    }
    Ok(records)
}

/// Resolve a full id or unique prefix to the dataset's full id.
pub fn dataset_resolve_id(id_or_prefix: &str) -> Result<String, StoreError> {
    dataset_resolve_id_at(&store_path(), id_or_prefix)
}

pub fn dataset_resolve_id_at(path: &Path, id_or_prefix: &str) -> Result<String, StoreError> {
    Ok(current(path, id_or_prefix)?.header.id)
}

/// Resolve `id_or_prefix` to its current commit, verified to be a live
/// dataset.
fn current(path: &Path, id_or_prefix: &str) -> Result<Object, StoreError> {
    let (id, sha) = resolve_id_at(path, id_or_prefix)?;
    let object = read_object_at(path, &sha)?;
    require_type(&object, OBJECT_TYPE)?;
    if object.header.is_tombstone() {
        return Err(StoreError::ObjectDeleted(id));
    }
    Ok(object)
}

fn members(object: &Object) -> Vec<String> {
    object
        .tree
        .links
        .get(SESSIONS_LINK)
        .cloned()
        .unwrap_or_default()
}

/// Metadata for one member session in a dataset, resolved through the
/// dataset's `sessions.link` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    /// 1-based index in `sessions.link`.
    pub session_num: u32,
    /// Native session id (`attrs.session_id`).
    pub session_id: String,
    pub driver_name: String,
    pub driver_version: String,
    pub session_type: SessionType,
    pub content_format: Option<String>,
}

/// Read a member session's metadata, resolving `session_ref` as either
/// a decimal `<n>` or a native session id.
pub fn dataset_session_meta(
    dataset_id: &str,
    session_ref: &str,
) -> Result<SessionMeta, StoreError> {
    dataset_session_meta_at(&store_path(), dataset_id, session_ref)
}

pub fn dataset_session_meta_at(
    path: &Path,
    dataset_id: &str,
    session_ref: &str,
) -> Result<SessionMeta, StoreError> {
    let members = members(&current(path, dataset_id)?);
    let session_num = resolve_session_num(path, &members, session_ref)?;
    let commit_sha = members
        .get((session_num - 1) as usize)
        .expect("resolve_session_num returns a valid 1-based index");
    let record = session_at_commit(path, commit_sha)?;
    Ok(SessionMeta {
        session_num,
        session_id: record.attrs.session_id,
        driver_name: record.driver_name,
        driver_version: record.driver_version,
        session_type: record.session_type,
        content_format: record.attrs.content_format,
    })
}

fn resolve_session_num(
    path: &Path,
    members: &[String],
    session_ref: &str,
) -> Result<u32, StoreError> {
    if let Ok(n) = session_ref.parse::<u32>() {
        if n == 0 || (n as usize) > members.len() {
            return Err(StoreError::SessionNotFound(session_ref.to_string()));
        }
        return Ok(n);
    }
    for (idx, sha) in members.iter().enumerate() {
        let record = session_at_commit(path, sha)?;
        if record.attrs.session_id == session_ref {
            return Ok((idx + 1) as u32);
        }
    }
    Err(StoreError::SessionNotFound(session_ref.to_string()))
}

/// Build a [`ContentAccess`] backed by git for the session at
/// position `session_num` in the given dataset.
pub fn dataset_session_content(
    dataset_id: &str,
    session_num: u32,
) -> Result<Box<dyn ContentAccess>, StoreError> {
    dataset_session_content_at(&store_path(), dataset_id, session_num)
}

pub fn dataset_session_content_at(
    path: &Path,
    dataset_id: &str,
    session_num: u32,
) -> Result<Box<dyn ContentAccess>, StoreError> {
    let members = members(&current(path, dataset_id)?);
    if session_num == 0 || (session_num as usize) > members.len() {
        return Err(StoreError::SessionNotFound(session_num.to_string()));
    }
    let session_commit = members
        .get((session_num - 1) as usize)
        .expect("bounds checked above")
        .clone();
    Ok(Box::new(GitContentAccess {
        store_path: path.to_path_buf(),
        session_commit,
    }))
}

struct GitContentAccess {
    store_path: std::path::PathBuf,
    session_commit: String,
}

impl ContentAccess for GitContentAccess {
    fn paths(&self) -> std::io::Result<Vec<String>> {
        let listing = run(git_in(
            &self.store_path,
            [
                "ls-tree",
                "-r",
                "--name-only",
                &format!("{}:files", self.session_commit),
            ],
        ))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(listing.lines().map(String::from).collect())
    }

    fn open(&self, path: &str) -> std::io::Result<Box<dyn Read + Send>> {
        let target = format!("{}:files/{}", self.session_commit, path);
        let mut cmd: Command = git_in(&self.store_path, ["cat-file", "-p", &target]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .expect("stdout was requested via Stdio::piped");
        Ok(Box::new(GitReader { child, stdout }))
    }
}

struct GitReader {
    child: std::process::Child,
    stdout: std::process::ChildStdout,
}

impl Read for GitReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Drop for GitReader {
    fn drop(&mut self) {
        // Reap the git process to avoid a zombie. The exit code is
        // uninteresting by the time the reader is dropped.
        drop(self.child.wait());
    }
}

/// Summary of one member session in a dataset, for list views.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetSessionSummary {
    pub session_num: u32,
    /// Native session id.
    pub session_id: String,
    /// Contents of the session's `attrs.session_type` (e.g. `"claude 1"`).
    pub session_type: String,
    /// Sum of blob sizes under the session's `files/**`.
    pub size: u64,
}

/// List sessions in the given dataset, ordered by ascending
/// `session_num`.
pub fn dataset_sessions_list(dataset_id: &str) -> Result<Vec<DatasetSessionSummary>, StoreError> {
    dataset_sessions_list_at(&store_path(), dataset_id)
}

pub fn dataset_sessions_list_at(
    path: &Path,
    dataset_id: &str,
) -> Result<Vec<DatasetSessionSummary>, StoreError> {
    let members = members(&current(path, dataset_id)?);
    let mut out = Vec::with_capacity(members.len());
    for (idx, session_commit) in members.iter().enumerate() {
        let record = session_at_commit(path, session_commit)?;
        out.push(DatasetSessionSummary {
            session_num: (idx + 1) as u32,
            session_id: record.attrs.session_id,
            session_type: record.attrs.session_type,
            size: record.size,
        });
    }
    Ok(out)
}

/// Outcome of adding one session to a dataset.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetSessionAddOutcome {
    /// Position in `sessions.link` after the operation.
    pub session_num: u32,
    /// Gage object id of the session.
    pub id: String,
    pub outcome: SessionOutcome,
}

/// One session to add to a dataset. Held for the duration of the call;
/// the caller owns the reader box.
pub struct SessionSpec<'a> {
    pub driver_name: &'a str,
    pub driver_version: &'a str,
    pub reader: &'a mut dyn SourceSession,
}

/// Add or update one or more sessions in the given dataset in a single
/// commit. Each spec is written as a session object (created, updated,
/// or unchanged); the dataset's `sessions.link` file is rewritten to
/// reference the resulting commit SHAs. When no member SHA changes, no
/// dataset commit is written.
pub fn dataset_sessions_add(
    dataset_id: &str,
    specs: Vec<SessionSpec<'_>>,
) -> Result<Vec<DatasetSessionAddOutcome>, StoreError> {
    dataset_sessions_add_at(&store_path(), dataset_id, specs)
}

pub fn dataset_sessions_add_at(
    path: &Path,
    dataset_id: &str,
    mut specs: Vec<SessionSpec<'_>>,
) -> Result<Vec<DatasetSessionAddOutcome>, StoreError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let dataset = current(path, dataset_id)?;
    let mut members = members(&dataset);

    // Object ids of existing members, so an update to a session already
    // in the dataset replaces its slot instead of appending.
    let mut member_ids: Vec<String> = Vec::with_capacity(members.len());
    for sha in &members {
        member_ids.push(read_object_at(path, sha)?.header.id);
    }

    let mut outcomes: Vec<DatasetSessionAddOutcome> = Vec::with_capacity(specs.len());
    for spec in specs.iter_mut() {
        let SessionAddOutcome {
            id,
            commit_sha,
            outcome,
        } = session_add_at(path, spec.driver_name, spec.driver_version, spec.reader)?;
        let session_num = match member_ids.iter().position(|m| m == &id) {
            Some(idx) => {
                *members
                    .get_mut(idx)
                    .expect("idx came from member_ids, which parallels members") = commit_sha;
                (idx + 1) as u32
            }
            None => {
                members.push(commit_sha);
                member_ids.push(id.clone());
                members.len() as u32
            }
        };
        outcomes.push(DatasetSessionAddOutcome {
            session_num,
            id,
            outcome,
        });
    }

    let mut tree = ObjectTree::default();
    tree.links.insert(SESSIONS_LINK.to_string(), members);
    let message = format_session_commit_message(&outcomes);
    match object::edit(path, &dataset, &tree, &message)? {
        EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(outcomes),
    }
}

fn format_session_commit_message(outcomes: &[DatasetSessionAddOutcome]) -> String {
    let mut added: Vec<u32> = Vec::new();
    let mut updated: Vec<u32> = Vec::new();
    for o in outcomes {
        match o.outcome {
            SessionOutcome::Added => added.push(o.session_num),
            SessionOutcome::Updated => updated.push(o.session_num),
            SessionOutcome::Unchanged => {}
        }
    }
    added.sort();
    updated.sort();
    let mut parts: Vec<String> = Vec::new();
    if !added.is_empty() {
        parts.push(format!("add {}", join_nums(&added)));
    }
    if !updated.is_empty() {
        parts.push(format!("update {}", join_nums(&updated)));
    }
    format!("sessions: {}", parts.join("; "))
}

fn join_nums(nums: &[u32]) -> String {
    nums.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_at;
    use crate::object::object_ref;
    use gage_session::{DriverError, SessionFile};
    use std::io::Cursor;

    fn init_store(dir: &Path) -> std::path::PathBuf {
        let store = dir.join("store.git");
        init_at(&store).unwrap();
        store
    }

    struct FakeSession {
        id: String,
        content: String,
    }

    impl SourceSession for FakeSession {
        fn session_id(&self) -> &str {
            &self.id
        }

        fn session_type(&self) -> &SessionType {
            static TYPE: std::sync::LazyLock<SessionType> =
                std::sync::LazyLock::new(|| SessionType::new("fake", "1"));
            &TYPE
        }

        fn content_format(&self) -> Option<&str> {
            None
        }

        fn files(&mut self) -> Box<dyn Iterator<Item = Result<SessionFile, DriverError>> + '_> {
            Box::new(std::iter::once(Ok(SessionFile {
                path: "session.jsonl".to_string(),
                content: Box::new(Cursor::new(self.content.clone().into_bytes())),
            })))
        }
    }

    fn fake(id: &str, content: &str) -> FakeSession {
        FakeSession {
            id: id.to_string(),
            content: content.to_string(),
        }
    }

    fn add(
        store: &Path,
        dataset: &str,
        sessions: &mut [FakeSession],
    ) -> Vec<DatasetSessionAddOutcome> {
        let specs = sessions
            .iter_mut()
            .map(|s| SessionSpec {
                driver_name: "fake",
                driver_version: "0.1",
                reader: s,
            })
            .collect();
        dataset_sessions_add_at(store, dataset, specs).unwrap()
    }

    fn rev_parse(store: &Path, id: &str) -> String {
        run(git_in(store, ["rev-parse", &object_ref(id)]))
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn new_writes_ref_and_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = dataset_new_at(&store).unwrap();
        assert_eq!(id.len(), 26);

        let ref_path = object_ref(&id);
        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["created", "id", "modified", "type"]
        );
        let type_content = run(git_in(
            &store,
            ["cat-file", "-p", &format!("{ref_path}:type")],
        ))
        .unwrap();
        assert_eq!(type_content, "gage::dataset 1\n");
        let id_content = run(git_in(
            &store,
            ["cat-file", "-p", &format!("{ref_path}:id")],
        ))
        .unwrap();
        assert_eq!(id_content, format!("{id}\n"));
    }

    #[test]
    fn list_reports_datasets_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let a = dataset_new_at(&store).unwrap();
        let b = dataset_new_at(&store).unwrap();
        crate::note_new_at(
            &store,
            crate::NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();

        let records = dataset_list_at(&store).unwrap();
        assert_eq!(records.len(), 2);
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&a.as_str()));
        assert!(ids.contains(&b.as_str()));
        for r in &records {
            assert!(r.created_ms > 0);
        }
    }

    #[test]
    fn list_empty_store_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        assert!(dataset_list_at(&store).unwrap().is_empty());
    }

    #[test]
    fn resolve_id_rejects_other_types() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let note = crate::note_new_at(
            &store,
            crate::NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        assert!(matches!(
            dataset_resolve_id_at(&store, &note).unwrap_err(),
            StoreError::WrongType { actual, .. } if actual == "gage::note"
        ));
    }

    #[test]
    fn sessions_add_links_members_as_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = dataset_new_at(&store).unwrap();
        let before = rev_parse(&store, &dataset);

        let outcomes = add(
            &store,
            &dataset,
            &mut [fake("s1", "a\n"), fake("s2", "b\n")],
        );
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].session_num, 1);
        assert_eq!(outcomes[1].session_num, 2);
        assert!(outcomes.iter().all(|o| o.outcome == SessionOutcome::Added));

        let tip = rev_parse(&store, &dataset);
        let meta = crate::git::read_commit_at(&store, &tip).unwrap();
        let s1 = rev_parse(&store, &outcomes[0].id);
        let s2 = rev_parse(&store, &outcomes[1].id);
        assert_eq!(meta.parents, vec![before.clone(), s1.clone(), s2.clone()]);
        assert!(
            meta.message.starts_with("sessions: add 1, 2"),
            "{}",
            meta.message
        );

        let link = run(git_in(
            &store,
            [
                "cat-file",
                "-p",
                &format!("{}:sessions.link", object_ref(&dataset)),
            ],
        ))
        .unwrap();
        assert_eq!(link, format!("{s1}\n{s2}\n"));

        let listed = dataset_sessions_list_at(&store, &dataset).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].session_id, "s1");
        assert_eq!(listed[1].session_id, "s2");
        assert_eq!(listed[0].session_type, "fake 1");
    }

    #[test]
    fn sessions_add_unchanged_writes_no_dataset_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = dataset_new_at(&store).unwrap();
        add(&store, &dataset, &mut [fake("s1", "a\n")]);
        let tip = rev_parse(&store, &dataset);

        let outcomes = add(&store, &dataset, &mut [fake("s1", "a\n")]);
        assert_eq!(outcomes[0].outcome, SessionOutcome::Unchanged);
        assert_eq!(rev_parse(&store, &dataset), tip);
    }

    #[test]
    fn sessions_add_updates_slot_of_grown_member() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = dataset_new_at(&store).unwrap();
        add(
            &store,
            &dataset,
            &mut [fake("s1", "a\n"), fake("s2", "b\n")],
        );

        let outcomes = add(&store, &dataset, &mut [fake("s1", "a\nmore\n")]);
        assert_eq!(outcomes[0].outcome, SessionOutcome::Updated);
        assert_eq!(outcomes[0].session_num, 1);

        let listed = dataset_sessions_list_at(&store, &dataset).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].size, 7);
        let meta = dataset_session_meta_at(&store, &dataset, "s1").unwrap();
        assert_eq!(meta.session_num, 1);
        assert_eq!(meta.driver_name, "fake");
        assert!(matches!(
            dataset_session_meta_at(&store, &dataset, "3").unwrap_err(),
            StoreError::SessionNotFound(s) if s == "3"
        ));
    }

    #[test]
    fn session_content_reads_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = dataset_new_at(&store).unwrap();
        add(&store, &dataset, &mut [fake("s1", "hello\n")]);

        let access = dataset_session_content_at(&store, &dataset, 1).unwrap();
        assert_eq!(access.paths().unwrap(), vec!["session.jsonl"]);
        let mut text = String::new();
        access
            .open("session.jsonl")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello\n");
    }
}
