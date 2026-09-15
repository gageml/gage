//! Dataset writer and reader.
//!
//! A dataset is a ref under `refs/gage/datasets/<id>`. Its tree carries
//! `format` (`gage-dataset 1\n`), `created`, and `modified`, and any
//! session directories added later. An `add` commit is parentless.

use std::collections::BTreeMap;
use std::path::Path;

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;
use gage_session::SourceSession;

use crate::writer::{commit_tree, mktree, write_blob, write_blob_stream};
use crate::{StoreError, exists, git_in, run, store_path};

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
    let format_sha = write_blob(path, b"gage-dataset 1\n")?;
    let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;

    let entries = vec![
        format!("100644 blob {stamp_sha}\tcreated"),
        format!("100644 blob {format_sha}\tformat"),
        format!("100644 blob {stamp_sha}\tmodified"),
    ];
    let tree_sha = mktree(path, &entries)?;

    let commit_sha = commit_tree(path, &tree_sha, "dataset", None)?;

    let ref_path = format!("refs/gage/datasets/{id}");
    run(git_in(path, ["update-ref", &ref_path, &commit_sha, ""]))?;

    Ok(id)
}

/// Summary of one session in a dataset, for list views.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetSessionSummary {
    /// The 1-based counter under `sessions/<n>/`.
    pub session_num: u32,
    /// Contents of the `session_id` file.
    pub session_id: String,
    /// Contents of the `session_type` file, e.g. `"claude 1"`.
    pub session_type: String,
    /// Sum of blob sizes under `sessions/<n>/content/**`.
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
    let commit = run(git_in(path, ["rev-parse", &ref_path]))?
        .trim()
        .to_string();
    let sessions = read_sessions_index(path, &commit)?;
    let mut out = Vec::with_capacity(sessions.len());
    for n in sessions.keys() {
        let session_id = run(git_in(
            path,
            [
                "cat-file",
                "-p",
                &format!("{commit}:sessions/{n}/session_id"),
            ],
        ))?
        .trim()
        .to_string();
        let session_type = run(git_in(
            path,
            [
                "cat-file",
                "-p",
                &format!("{commit}:sessions/{n}/session_type"),
            ],
        ))?
        .trim()
        .to_string();
        let size = content_bytes(path, &commit, *n)?;
        out.push(DatasetSessionSummary {
            session_num: *n,
            session_id,
            session_type,
            size,
        });
    }
    Ok(out)
}

/// Sum of blob sizes under `sessions/<n>/content/`. Zero when the
/// content subtree is absent.
fn content_bytes(path: &Path, commit: &str, session_num: u32) -> Result<u64, StoreError> {
    let listing = match run(git_in(
        path,
        [
            "ls-tree",
            "-r",
            "-l",
            &format!("{commit}:sessions/{session_num}/content"),
        ],
    )) {
        Ok(s) => s,
        Err(StoreError::Git { .. }) => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut total = 0u64;
    for line in listing.lines() {
        let (meta, _) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let parts: Vec<&str> = meta.split_whitespace().collect();
        let [_, kind, _, size_str]: [&str; 4] = parts.try_into().map_err(|got: Vec<&str>| {
            StoreError::Parse(format!("ls-tree meta {meta:?}: got {}", got.len()))
        })?;
        if kind != "blob" {
            continue;
        }
        total += size_str
            .parse::<u64>()
            .map_err(|e| StoreError::Parse(format!("ls-tree size {size_str:?}: {e}")))?;
    }
    Ok(total)
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

/// Outcome of a session add: which slot the session landed in and
/// whether anything changed.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionAddOutcome {
    pub session_num: u32,
    pub outcome: SessionOutcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// New session dir at `sessions/<n>/`.
    Added,
    /// Existing session dir replaced with new content.
    Updated,
    /// Existing session matched byte-for-byte; no commit was written.
    NoOp,
}

/// One session to add to a dataset. Held for the duration of the
/// call; the caller owns the reader box.
pub struct SessionSpec<'a> {
    pub driver_name: &'a str,
    pub driver_version: &'a str,
    pub reader: &'a mut dyn SourceSession,
}

/// Add or update one or more sessions in the given dataset in a single
/// commit. Streams each session reader's files into the store, splices
/// each session dir into the dataset tree at `sessions/<n>/`, and
/// chains one new commit for the whole batch. If every input is a
/// byte-identical no-op, no commit is written.
pub fn dataset_sessions_add(
    dataset_id: &str,
    specs: Vec<SessionSpec<'_>>,
) -> Result<Vec<SessionAddOutcome>, StoreError> {
    dataset_sessions_add_at(&store_path(), dataset_id, specs)
}

pub fn dataset_sessions_add_at(
    path: &Path,
    dataset_id: &str,
    mut specs: Vec<SessionSpec<'_>>,
) -> Result<Vec<SessionAddOutcome>, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let ref_path = format!("refs/gage/datasets/{dataset_id}");
    let current_commit = run(git_in(path, ["rev-parse", &ref_path]))?
        .trim()
        .to_string();

    let existing_sessions = read_sessions_index(path, &current_commit)?;
    // Pending state accumulates as we process each spec: existing
    // slot shas overridden by newly built trees, next number bumps
    // as new slots are allocated.
    let mut pending: BTreeMap<u32, String> = existing_sessions.clone();
    let mut next_num = existing_sessions.keys().copied().max().map_or(1, |m| m + 1);
    let mut outcomes: Vec<SessionAddOutcome> = Vec::with_capacity(specs.len());
    let mut any_change = false;

    for spec in specs.iter_mut() {
        let existing_hit = find_matching_session(
            path,
            &current_commit,
            &existing_sessions,
            spec.driver_name,
            spec.reader.session_id(),
        )?;
        let session_num = match existing_hit {
            Some(n) => n,
            None => {
                let n = next_num;
                next_num += 1;
                n
            }
        };
        let session_tree_sha =
            build_session_tree(path, spec.driver_name, spec.driver_version, spec.reader)?;
        let previous_sha = existing_sessions.get(&session_num);
        let outcome = if previous_sha == Some(&session_tree_sha) {
            SessionOutcome::NoOp
        } else if existing_hit.is_some() {
            SessionOutcome::Updated
        } else {
            SessionOutcome::Added
        };
        if outcome != SessionOutcome::NoOp {
            pending.insert(session_num, session_tree_sha);
            any_change = true;
        }
        outcomes.push(SessionAddOutcome {
            session_num,
            outcome,
        });
    }

    if !any_change {
        return Ok(outcomes);
    }

    // Rebuild sessions/ subtree from the pending map.
    let sessions_entries: Vec<(String, String)> = pending
        .iter()
        .map(|(n, sha)| (n.to_string(), sha.clone()))
        .collect();
    let sessions_tree_sha = mktree_dirs(path, &sessions_entries)?;

    // Rewrite the top-level tree.
    let top = read_top_tree(path, &current_commit)?;
    let created_sha = top.get("created").cloned().ok_or_else(|| {
        StoreError::Parse(format!("dataset {dataset_id}: missing top-level `created`"))
    })?;
    let format_sha = top.get("format").cloned().ok_or_else(|| {
        StoreError::Parse(format!("dataset {dataset_id}: missing top-level `format`"))
    })?;
    let now = now_ms();
    let modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;
    let mut top_entries = vec![
        format!("100644 blob {created_sha}\tcreated"),
        format!("100644 blob {format_sha}\tformat"),
        format!("100644 blob {modified_sha}\tmodified"),
        format!("040000 tree {sessions_tree_sha}\tsessions"),
    ];
    top_entries.sort_by(|a, b| {
        let na = a.split('\t').nth(1).unwrap_or("");
        let nb = b.split('\t').nth(1).unwrap_or("");
        na.cmp(nb)
    });
    let top_tree_sha = mktree(path, &top_entries)?;

    let message = format_session_commit_message(&outcomes);
    let new_commit = commit_tree(path, &top_tree_sha, &message, Some(&current_commit))?;
    run(git_in(
        path,
        ["update-ref", &ref_path, &new_commit, &current_commit],
    ))?;

    Ok(outcomes)
}

fn format_session_commit_message(outcomes: &[SessionAddOutcome]) -> String {
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

/// Return `<n> -> tree sha` for every existing `sessions/<n>/` entry.
fn read_sessions_index(path: &Path, commit: &str) -> Result<BTreeMap<u32, String>, StoreError> {
    // Peek at the top-level tree; if `sessions` is absent, empty map.
    let top_listing = run(git_in(path, ["ls-tree", commit]))?;
    let has_sessions = top_listing.lines().any(|l| {
        l.split_once('\t')
            .map(|(_, n)| n == "sessions")
            .unwrap_or(false)
    });
    if !has_sessions {
        return Ok(BTreeMap::new());
    }
    let listing = run(git_in(path, ["ls-tree", &format!("{commit}:sessions")]))?;
    let mut out = BTreeMap::new();
    for line in listing.lines() {
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree sessions: {line}")))?;
        let parts: Vec<&str> = meta.split_whitespace().collect();
        let [_, kind, sha]: [&str; 3] = parts.try_into().map_err(|got: Vec<&str>| {
            StoreError::Parse(format!("ls-tree sessions meta {meta:?}: got {}", got.len()))
        })?;
        if kind != "tree" {
            return Err(StoreError::Parse(format!(
                "unexpected entry under sessions/: {name} ({kind})"
            )));
        }
        let n: u32 = name
            .parse()
            .map_err(|e| StoreError::Parse(format!("session dir name {name:?}: {e}")))?;
        out.insert(n, sha.to_string());
    }
    Ok(out)
}

/// Find the existing session `<n>` whose `(driver_name, session_id)`
/// matches the incoming pair, if any.
fn find_matching_session(
    path: &Path,
    commit: &str,
    existing: &BTreeMap<u32, String>,
    driver_name: &str,
    session_id: &str,
) -> Result<Option<u32>, StoreError> {
    for n in existing.keys() {
        let driver = run(git_in(
            path,
            ["cat-file", "-p", &format!("{commit}:sessions/{n}/driver")],
        ))?;
        let existing_driver_name = driver.split_whitespace().next().unwrap_or("");
        if existing_driver_name != driver_name {
            continue;
        }
        let existing_id = run(git_in(
            path,
            [
                "cat-file",
                "-p",
                &format!("{commit}:sessions/{n}/session_id"),
            ],
        ))?;
        if existing_id.trim() == session_id {
            return Ok(Some(*n));
        }
    }
    Ok(None)
}

/// Read the top-level tree of `commit` into a map of `name -> sha`.
fn read_top_tree(path: &Path, commit: &str) -> Result<BTreeMap<String, String>, StoreError> {
    let listing = run(git_in(path, ["ls-tree", commit]))?;
    let mut out = BTreeMap::new();
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

/// Build the tree for one `sessions/<n>/` directory. Streams the
/// reader's files into the store, then assembles the metadata blobs
/// plus the `content/` subtree.
fn build_session_tree(
    path: &Path,
    driver_name: &str,
    driver_version: &str,
    reader: &mut dyn SourceSession,
) -> Result<String, StoreError> {
    let session_id_sha = write_blob(path, format!("{}\n", reader.session_id()).as_bytes())?;
    let driver_sha = write_blob(path, format!("{driver_name} {driver_version}\n").as_bytes())?;
    let session_type_sha = write_blob(path, format!("{}\n", reader.session_type()).as_bytes())?;
    let content_format_sha = reader
        .content_format()
        .map(|s| write_blob(path, format!("{s}\n").as_bytes()))
        .transpose()?;

    let mut content_entries: Vec<(String, String)> = Vec::new();
    for file in reader.files() {
        let file = file.map_err(|e| StoreError::Parse(format!("driver: {e}")))?;
        let sha = write_blob_stream(path, file.content)?;
        content_entries.push((file.path, sha));
    }
    let content_tree_sha = build_content_tree(path, content_entries)?;

    let mut entries: Vec<String> = vec![
        format!("040000 tree {content_tree_sha}\tcontent"),
        format!("100644 blob {driver_sha}\tdriver"),
        format!("100644 blob {session_id_sha}\tsession_id"),
        format!("100644 blob {session_type_sha}\tsession_type"),
    ];
    if let Some(sha) = &content_format_sha {
        entries.push(format!("100644 blob {sha}\tcontent_format"));
    }
    entries.sort_by(|a, b| {
        let na = a.split('\t').nth(1).unwrap_or("");
        let nb = b.split('\t').nth(1).unwrap_or("");
        na.cmp(nb)
    });
    mktree(path, &entries)
}

/// Recursively build a tree from `(relative_path, blob_sha)` pairs.
fn build_content_tree(path: &Path, entries: Vec<(String, String)>) -> Result<String, StoreError> {
    let mut blobs: BTreeMap<String, String> = BTreeMap::new();
    let mut subdirs: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (rel, sha) in entries {
        match rel.split_once('/') {
            Some((dir, rest)) => {
                subdirs
                    .entry(dir.to_string())
                    .or_default()
                    .push((rest.to_string(), sha));
            }
            None => {
                blobs.insert(rel, sha);
            }
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for (name, sha) in blobs {
        lines.push(format!("100644 blob {sha}\t{name}"));
    }
    for (dir, sub) in subdirs {
        let sub_sha = build_content_tree(path, sub)?;
        lines.push(format!("040000 tree {sub_sha}\t{dir}"));
    }
    lines.sort_by(|a, b| {
        let na = a.split('\t').nth(1).unwrap_or("");
        let nb = b.split('\t').nth(1).unwrap_or("");
        na.cmp(nb)
    });
    mktree(path, &lines)
}

/// Build a tree whose entries are numbered subdirectories, each
/// pointing at the given tree sha.
fn mktree_dirs(path: &Path, entries: &[(String, String)]) -> Result<String, StoreError> {
    let mut lines: Vec<String> = entries
        .iter()
        .map(|(name, sha)| format!("040000 tree {sha}\t{name}"))
        .collect();
    lines.sort_by(|a, b| {
        let na = a.split('\t').nth(1).unwrap_or("");
        let nb = b.split('\t').nth(1).unwrap_or("");
        na.cmp(nb)
    });
    mktree(path, &lines)
}

/// A dataset summary row for the list view.
#[derive(Debug, PartialEq, Eq)]
pub struct DatasetRecord {
    pub id: String,
    pub created_ms: i64,
}

/// List every dataset in the default store, newest first by
/// committer date.
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
        assert_eq!(names, vec!["created", "format", "modified"]);

        let format_content = run(git_in(
            &store,
            ["cat-file", "-p", &format!("{ref_path}:format")],
        ))
        .unwrap();
        assert_eq!(format_content, "gage-dataset 1\n");
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
