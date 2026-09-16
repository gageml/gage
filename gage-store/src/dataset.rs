//! Dataset writer and reader.
//!
//! A dataset is a ref under `refs/gage/datasets/<id>`. Its tree carries
//! the common header (`object` = `gage::dataset 1\n`, `id`, `created`,
//! `modified`) and, once sessions have been added, a `sessions.link`
//! blob listing the commit SHA of each member session, one per line,
//! in insertion order. Every SHA in `sessions.link` becomes a commit
//! parent of the dataset commit, so pushing a dataset carries its
//! sessions along. Session content itself lives in first-class
//! [`gage::session`](crate::session) objects; the dataset does not
//! copy it.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_session::{ContentAccess, SessionType, SourceSession};

use crate::git::{git_in, run};
use crate::session::{SessionAddOutcome, SessionOutcome, session_add_at, session_at_commit};
use crate::writer::{commit_tree, mktree, write_blob};
use crate::{StoreError, exists, store_path};

/// `object` blob content for a dataset tree.
const DATASET_OBJECT: &[u8] = b"gage::dataset 1\n";

/// Add an empty dataset to the default store. Returns the new id.
pub fn dataset_add() -> Result<String, StoreError> {
    dataset_add_at(&store_path())
}

/// Add an empty dataset to the store at `path`. Returns the new id.
pub fn dataset_add_at(path: &Path) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let id = new_uuid();
    let now = now_ms();
    let object_sha = write_blob(path, DATASET_OBJECT)?;
    let id_sha = write_blob(path, format!("{id}\n").as_bytes())?;
    let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;

    let entries = vec![
        format!("100644 blob {stamp_sha}\tcreated"),
        format!("100644 blob {id_sha}\tid"),
        format!("100644 blob {stamp_sha}\tmodified"),
        format!("100644 blob {object_sha}\tobject"),
    ];
    let tree_sha = mktree(path, &entries)?;

    let commit_sha = commit_tree(path, &tree_sha, "dataset", &[])?;

    let ref_path = format!("refs/gage/datasets/{id}");
    run(git_in(path, ["update-ref", &ref_path, &commit_sha, ""]))?;

    Ok(id)
}

/// Metadata for one member session in a dataset, resolved through the
/// dataset's `sessions.link` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    /// 1-based index in `sessions.link`.
    pub session_num: u32,
    /// The source's own session id (attrs.session_id).
    pub session_id: String,
    pub driver_name: String,
    pub driver_version: String,
    pub session_type: SessionType,
    pub content_format: Option<String>,
}

/// Read a session's metadata, resolving `session_ref` as either a
/// decimal `<n>` or a source `session_id` string.
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
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let ref_path = format!("refs/gage/datasets/{dataset_id}");
    let commit = rev_parse(path, &ref_path)?;
    let members = read_sessions_link(path, &commit)?;
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
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let ref_path = format!("refs/gage/datasets/{dataset_id}");
    let commit = rev_parse(path, &ref_path)?;
    let members = read_sessions_link(path, &commit)?;
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
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let ref_path = format!("refs/gage/datasets/{dataset_id}");
    let commit = rev_parse(path, &ref_path)?;
    let members = read_sessions_link(path, &commit)?;
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

/// Resolve a full id or unique prefix to the dataset's full id.
pub fn dataset_resolve_id(id_or_prefix: &str) -> Result<String, StoreError> {
    dataset_resolve_id_at(&store_path(), id_or_prefix)
}

pub fn dataset_resolve_id_at(path: &Path, id_or_prefix: &str) -> Result<String, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let pattern = format!("refs/gage/datasets/{id_or_prefix}*");
    let matches = run(git_in(
        path,
        ["for-each-ref", "--format=%(refname:strip=3)", &pattern],
    ))?;
    let ids: Vec<&str> = matches.lines().collect();
    match ids.as_slice() {
        [] => Err(StoreError::DatasetNotFound(id_or_prefix.to_string())),
        [only] => Ok((*only).to_string()),
        many => Err(StoreError::AmbiguousDatasetId(
            id_or_prefix.to_string(),
            many.len(),
        )),
    }
}

/// Outcome of adding one session to a dataset.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetSessionAddOutcome {
    /// Position in `sessions.link` after the operation.
    pub session_num: u32,
    /// Derived Gage session object id.
    pub session_id: String,
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
/// commit. Each spec is materialized as a session object (created,
/// updated, or reused); the dataset's `sessions.link` file is rewritten
/// to reference the resulting commit SHAs. If every input is a
/// byte-identical no-op and the linked SHAs are unchanged, no dataset
/// commit is written.
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
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let ref_path = format!("refs/gage/datasets/{dataset_id}");
    let current_commit = rev_parse(path, &ref_path)?;
    let mut members = read_sessions_link(path, &current_commit)?;

    // Compute derived ids of existing members so we can detect updates
    // to a session already in the dataset without re-scanning.
    let mut member_ids: Vec<String> = Vec::with_capacity(members.len());
    for sha in &members {
        member_ids.push(read_id_blob(path, sha)?);
    }

    let mut outcomes: Vec<DatasetSessionAddOutcome> = Vec::with_capacity(specs.len());
    let mut sessions_link_changed = false;

    for spec in specs.iter_mut() {
        let SessionAddOutcome {
            id,
            commit_sha,
            outcome,
        } = session_add_at(path, spec.driver_name, spec.driver_version, spec.reader)?;
        let position = member_ids.iter().position(|m| m == &id);
        let session_num = match position {
            Some(idx) => {
                let slot = members
                    .get_mut(idx)
                    .expect("idx came from members.iter().position");
                if *slot != commit_sha {
                    *slot = commit_sha.clone();
                    sessions_link_changed = true;
                }
                (idx + 1) as u32
            }
            None => {
                members.push(commit_sha.clone());
                member_ids.push(id.clone());
                sessions_link_changed = true;
                members.len() as u32
            }
        };
        outcomes.push(DatasetSessionAddOutcome {
            session_num,
            session_id: id,
            outcome,
        });
    }

    if !sessions_link_changed {
        return Ok(outcomes);
    }

    // Rebuild the dataset tree with a fresh `sessions.link` and `prev`.
    let sessions_link_content: String = members.iter().map(|s| format!("{s}\n")).collect();
    let sessions_link_sha = write_blob(path, sessions_link_content.as_bytes())?;

    let top = read_top_tree(path, &current_commit)?;
    let created_sha = top
        .get("created")
        .cloned()
        .ok_or_else(|| StoreError::Parse(format!("dataset {dataset_id}: missing `created`")))?;
    let id_sha = top
        .get("id")
        .cloned()
        .ok_or_else(|| StoreError::Parse(format!("dataset {dataset_id}: missing `id`")))?;
    let object_sha = top
        .get("object")
        .cloned()
        .ok_or_else(|| StoreError::Parse(format!("dataset {dataset_id}: missing `object`")))?;
    let now = now_ms();
    let modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;
    let prev_sha = write_blob(path, format!("{current_commit}\n").as_bytes())?;

    let mut top_entries = vec![
        format!("100644 blob {created_sha}\tcreated"),
        format!("100644 blob {id_sha}\tid"),
        format!("100644 blob {modified_sha}\tmodified"),
        format!("100644 blob {object_sha}\tobject"),
        format!("100644 blob {prev_sha}\tprev"),
        format!("100644 blob {sessions_link_sha}\tsessions.link"),
    ];
    top_entries.sort_by(|a, b| tree_entry_name(a).cmp(tree_entry_name(b)));
    let top_tree_sha = mktree(path, &top_entries)?;

    let mut parents: Vec<&str> = vec![&current_commit];
    parents.extend(members.iter().map(|s| s.as_str()));

    let message = format_session_commit_message(&outcomes);
    let new_commit = commit_tree(path, &top_tree_sha, &message, &parents)?;
    run(git_in(
        path,
        ["update-ref", &ref_path, &new_commit, &current_commit],
    ))?;

    Ok(outcomes)
}

fn format_session_commit_message(outcomes: &[DatasetSessionAddOutcome]) -> String {
    let mut added: Vec<u32> = Vec::new();
    let mut updated: Vec<u32> = Vec::new();
    for o in outcomes {
        match o.outcome {
            SessionOutcome::Added => added.push(o.session_num),
            SessionOutcome::Updated => updated.push(o.session_num),
            SessionOutcome::NoOp => {}
        }
    }
    added.sort();
    updated.sort();
    let mut parts: Vec<String> = Vec::new();
    if !added.is_empty() {
        parts.push(format!(
            "add {}",
            added
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !updated.is_empty() {
        parts.push(format!(
            "update {}",
            updated
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    format!("sessions: {}", parts.join("; "))
}

fn read_sessions_link(path: &Path, commit: &str) -> Result<Vec<String>, StoreError> {
    let listing = run(git_in(path, ["ls-tree", commit]))?;
    if !listing
        .lines()
        .any(|l| l.split('\t').nth(1) == Some("sessions.link"))
    {
        return Ok(Vec::new());
    }
    let content = run(git_in(
        path,
        ["cat-file", "-p", &format!("{commit}:sessions.link")],
    ))?;
    Ok(content.lines().map(|s| s.trim().to_string()).collect())
}

fn read_id_blob(path: &Path, commit_sha: &str) -> Result<String, StoreError> {
    let s = run(git_in(
        path,
        ["cat-file", "-p", &format!("{commit_sha}:id")],
    ))?;
    Ok(s.trim().to_string())
}

fn read_top_tree(
    path: &Path,
    commit: &str,
) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
    let listing = run(git_in(path, ["ls-tree", commit]))?;
    let mut out = std::collections::BTreeMap::new();
    for line in listing.lines() {
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let sha = meta
            .split_whitespace()
            .nth(2)
            .ok_or_else(|| StoreError::Parse(format!("ls-tree meta: {meta}")))?;
        out.insert(name.to_string(), sha.to_string());
    }
    Ok(out)
}

fn rev_parse(path: &Path, ref_path: &str) -> Result<String, StoreError> {
    Ok(run(git_in(path, ["rev-parse", ref_path]))?
        .trim()
        .to_string())
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

fn tree_entry_name(entry: &str) -> &str {
    entry.split('\t').nth(1).unwrap_or("")
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

pub fn dataset_list_at(path: &Path) -> Result<Vec<DatasetRecord>, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let listing = run(git_in(
        path,
        [
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:strip=3)",
            "refs/gage/datasets/",
        ],
    ))?;

    let mut records = Vec::new();
    for id in listing.lines() {
        let ref_path = format!("refs/gage/datasets/{id}");
        let created = run(git_in(
            path,
            ["cat-file", "-p", &format!("{ref_path}:created")],
        ))?;
        let created_ms: i64 = created
            .trim()
            .parse()
            .map_err(|e| StoreError::Parse(format!("created {id}: {e}")))?;
        records.push(DatasetRecord {
            id: id.to_string(),
            created_ms,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_at;

    fn init_store(dir: &Path) -> std::path::PathBuf {
        let store = dir.join("store.git");
        init_at(&store).unwrap();
        store
    }

    #[test]
    fn add_writes_ref_and_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = dataset_add_at(&store).unwrap();
        assert_eq!(id.len(), 26);

        let ref_path = format!("refs/gage/datasets/{id}");
        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
        let names: Vec<&str> = listing.lines().collect();
        assert_eq!(names, vec!["created", "id", "modified", "object"]);

        let object_content = run(git_in(
            &store,
            ["cat-file", "-p", &format!("{ref_path}:object")],
        ))
        .unwrap();
        assert_eq!(object_content, "gage::dataset 1\n");

        let id_content = run(git_in(
            &store,
            ["cat-file", "-p", &format!("{ref_path}:id")],
        ))
        .unwrap();
        assert_eq!(id_content, format!("{id}\n"));
    }

    #[test]
    fn list_reports_added_datasets() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let a = dataset_add_at(&store).unwrap();
        let b = dataset_add_at(&store).unwrap();

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
}
