//! Session objects. A session is a first-class Gage object stored at
//! `refs/gage/sessions/<id>`. The tree carries the common header
//! (`object` = `gage::session 1\n`, `id`, `created`, `modified`),
//! `attrs` (compact JSON of driver, namespace, source session id,
//! session type, and optional content format), and `files/**` (the
//! session content as provided by the driver). An `add` for a new
//! session is a parentless commit; a subsequent add for the same
//! derived id whose content has changed writes an edit commit with a
//! `prev` blob and lineage parent. When content is byte-identical no
//! commit is written and the existing SHA is returned.

use std::collections::BTreeMap;
use std::path::Path;

use gage_core::datetime::now_ms;
use gage_core::uuid::derive_id;
use gage_session::{SessionType, SourceSession};
use serde::{Deserialize, Serialize};

use crate::git::{git_in, run};
use crate::writer::{commit_tree, mktree, write_blob, write_blob_stream};
use crate::{StoreError, exists};

/// `object` blob content for a session tree.
const SESSION_OBJECT: &[u8] = b"gage::session 1\n";

/// Persisted `attrs` for a session tree.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionAttrs {
    pub driver: String,
    pub session_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_format: Option<String>,
    pub namespace: String,
    pub session_id: String,
}

/// Outcome of writing one session to the store.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionAddOutcome {
    /// Derived Gage object id for the session.
    pub id: String,
    /// Commit SHA of the resulting version. When `outcome` is
    /// [`SessionOutcome::NoOp`] this is the existing commit.
    pub commit_sha: String,
    pub outcome: SessionOutcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// New session object created.
    Added,
    /// Existing session object updated with new content.
    Updated,
    /// Existing session content matched byte-for-byte; no commit was
    /// written.
    NoOp,
}

/// Derive the Gage session id from a driver name and source session id
/// tuple. The namespace is empty for every driver today.
pub fn session_id_for(driver_name: &str, session_id: &str) -> String {
    let namespace = "";
    derive_id(&format!(
        "session\0{driver_name}\0{namespace}\0{session_id}"
    ))
}

/// Write `reader`'s content as a session object under
/// `refs/gage/sessions/<id>`. Idempotent when the source's files are
/// unchanged.
pub fn session_add_at(
    path: &Path,
    driver_name: &str,
    driver_version: &str,
    reader: &mut dyn SourceSession,
) -> Result<SessionAddOutcome, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let source_session_id = reader.session_id().to_string();
    let id = session_id_for(driver_name, &source_session_id);
    let ref_path = format!("refs/gage/sessions/{id}");

    let attrs = SessionAttrs {
        driver: format!("{driver_name} {driver_version}"),
        session_type: reader.session_type().to_string(),
        content_format: reader.content_format().map(str::to_string),
        namespace: String::new(),
        session_id: source_session_id,
    };
    let attrs_sha = write_blob(path, serialize_attrs(&attrs).as_bytes())?;

    let mut file_entries: Vec<(String, String)> = Vec::new();
    for file in reader.files() {
        let file = file.map_err(|e| StoreError::Parse(format!("driver: {e}")))?;
        let sha = write_blob_stream(path, file.content)?;
        file_entries.push((file.path, sha));
    }
    let files_tree_sha = build_files_tree(path, file_entries)?;

    let existing = read_current(path, &ref_path)?;
    if let Some(existing) = &existing
        && existing.attrs_sha == attrs_sha
        && existing.files_tree_sha == files_tree_sha
    {
        return Ok(SessionAddOutcome {
            id,
            commit_sha: existing.commit_sha.clone(),
            outcome: SessionOutcome::NoOp,
        });
    }

    let now = now_ms();
    let object_sha = write_blob(path, SESSION_OBJECT)?;
    let id_blob_sha = write_blob(path, format!("{id}\n").as_bytes())?;
    let modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;
    let (created_sha, prev_sha) = match &existing {
        Some(e) => (
            e.created_sha.clone(),
            Some(write_blob(path, format!("{}\n", e.commit_sha).as_bytes())?),
        ),
        None => (modified_sha.clone(), None),
    };

    let mut entries = vec![
        format!("100644 blob {attrs_sha}\tattrs"),
        format!("100644 blob {created_sha}\tcreated"),
        format!("040000 tree {files_tree_sha}\tfiles"),
        format!("100644 blob {id_blob_sha}\tid"),
        format!("100644 blob {modified_sha}\tmodified"),
        format!("100644 blob {object_sha}\tobject"),
    ];
    if let Some(prev) = &prev_sha {
        entries.push(format!("100644 blob {prev}\tprev"));
    }
    entries.sort_by(|a, b| tree_entry_name(a).cmp(tree_entry_name(b)));
    let tree_sha = mktree(path, &entries)?;

    let (parents, message, outcome, expected) = match &existing {
        Some(e) => (
            vec![e.commit_sha.as_str()],
            format!("session edit: {driver_name}:{}", attrs.session_id),
            SessionOutcome::Updated,
            e.commit_sha.clone(),
        ),
        None => (
            Vec::new(),
            format!("session: {driver_name}:{}", attrs.session_id),
            SessionOutcome::Added,
            String::new(),
        ),
    };
    let commit_sha = commit_tree(path, &tree_sha, &message, &parents)?;
    run(git_in(
        path,
        ["update-ref", &ref_path, &commit_sha, &expected],
    ))?;

    Ok(SessionAddOutcome {
        id,
        commit_sha,
        outcome,
    })
}

/// A store session presented for reading, resolved to its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub commit_sha: String,
    pub attrs: SessionAttrs,
    pub session_type: SessionType,
    pub driver_name: String,
    pub driver_version: String,
    /// Total bytes of blobs under `files/**`.
    pub size: u64,
}

/// Read the session at the given commit SHA into a [`SessionRecord`].
pub fn session_at_commit(path: &Path, commit_sha: &str) -> Result<SessionRecord, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let id = run(git_in(
        path,
        ["cat-file", "-p", &format!("{commit_sha}:id")],
    ))?
    .trim()
    .to_string();
    let attrs_json = run(git_in(
        path,
        ["cat-file", "-p", &format!("{commit_sha}:attrs")],
    ))?;
    let attrs: SessionAttrs = serde_json::from_str(attrs_json.trim_end())
        .map_err(|e| StoreError::Parse(format!("session attrs {commit_sha}: {e}")))?;
    let (driver_name, driver_version) = match attrs.driver.split_once(' ') {
        Some((n, v)) => (n.to_string(), v.to_string()),
        None => (attrs.driver.clone(), String::new()),
    };
    let session_type = match attrs.session_type.split_once(' ') {
        Some((n, v)) => SessionType::new(n.to_string(), v.to_string()),
        None => SessionType::new(attrs.session_type.clone(), String::new()),
    };
    let size = files_size(path, commit_sha)?;
    Ok(SessionRecord {
        id,
        commit_sha: commit_sha.to_string(),
        attrs,
        session_type,
        driver_name,
        driver_version,
        size,
    })
}

/// Sum of blob sizes under `<commit_sha>:files/`.
pub(crate) fn files_size(path: &Path, commit_sha: &str) -> Result<u64, StoreError> {
    let listing = match run(git_in(
        path,
        ["ls-tree", "-r", "-l", &format!("{commit_sha}:files")],
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
            StoreError::Parse(format!("ls-tree meta: got {}", got.len()))
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

/// Blob and tree SHAs of the current session commit, for idempotency
/// comparison.
struct Current {
    commit_sha: String,
    attrs_sha: String,
    files_tree_sha: String,
    created_sha: String,
}

fn read_current(path: &Path, ref_path: &str) -> Result<Option<Current>, StoreError> {
    let commit_sha = match run(git_in(path, ["rev-parse", "--verify", ref_path])) {
        Ok(s) => s.trim().to_string(),
        Err(StoreError::Git { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let listing = run(git_in(path, ["ls-tree", &commit_sha]))?;
    let mut attrs_sha = None;
    let mut files_tree_sha = None;
    let mut created_sha = None;
    for line in listing.lines() {
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let sha = meta
            .split_whitespace()
            .nth(2)
            .ok_or_else(|| StoreError::Parse(format!("ls-tree meta: {meta}")))?;
        match name {
            "attrs" => attrs_sha = Some(sha.to_string()),
            "files" => files_tree_sha = Some(sha.to_string()),
            "created" => created_sha = Some(sha.to_string()),
            _ => {}
        }
    }
    Ok(Some(Current {
        commit_sha,
        attrs_sha: attrs_sha
            .ok_or_else(|| StoreError::Parse(format!("missing attrs in {ref_path}")))?,
        files_tree_sha: files_tree_sha
            .ok_or_else(|| StoreError::Parse(format!("missing files in {ref_path}")))?,
        created_sha: created_sha
            .ok_or_else(|| StoreError::Parse(format!("missing created in {ref_path}")))?,
    }))
}

/// Recursively build the `files/` tree from `(relative_path, blob_sha)`
/// pairs.
fn build_files_tree(path: &Path, entries: Vec<(String, String)>) -> Result<String, StoreError> {
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
        let sub_sha = build_files_tree(path, sub)?;
        lines.push(format!("040000 tree {sub_sha}\t{dir}"));
    }
    lines.sort_by(|a, b| tree_entry_name(a).cmp(tree_entry_name(b)));
    mktree(path, &lines)
}

fn tree_entry_name(entry: &str) -> &str {
    entry.split('\t').nth(1).unwrap_or("")
}

fn serialize_attrs(attrs: &SessionAttrs) -> String {
    let mut s = serde_json::to_string(attrs).expect("session attrs are always serializable");
    s.push('\n');
    s
}
