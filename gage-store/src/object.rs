//! Structural, payload-agnostic reads over the Gage object graph.
//!
//! An object is a commit under `refs/gage/<bucket>/<id>` whose tree
//! carries a common header: `object` (`<type> <version>`), `id`,
//! `created`, `modified`, optionally `deleted` (tombstone) and
//! optionally `prev` (previous version). Additional blobs whose path
//! ends in `.link` list SHAs that are also commit parents. This
//! module knows those files and nothing else --- it never parses
//! `attrs` or interprets any per-type payload.
//!
//! The Git primitives these reads compose from live in [`crate::git`].

use std::path::Path;

use crate::git::{git_in, read_commit_at, run};
use crate::{StoreError, store_path};

/// One ref under `refs/gage/`, split into its type bucket and id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    /// Full ref name, e.g. `refs/gage/notes/<id>`.
    pub ref_name: String,
    /// Segment immediately after `refs/gage/`, e.g. `notes`.
    pub type_bucket: String,
    /// The object id --- everything after `refs/gage/<bucket>/`.
    pub id: String,
    /// SHA the ref points to.
    pub tip_sha: String,
}

/// The four common header files plus the two optional markers a reader
/// classifies by, all in one read. `object` and `id` are required; a
/// tombstone has `deleted` set and no `attrs`, so nothing here parses
/// `attrs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectHeader {
    /// The `<type>` half of the `object` blob (`gage::note` etc.).
    pub object_type: String,
    /// The `<version>` half of the `object` blob (`1` etc.).
    pub version: String,
    /// Contents of the `id` blob.
    pub id: String,
    /// UNIX time millis from `created`, when present.
    pub created_ms: Option<i64>,
    /// UNIX time millis from `modified`, when present.
    pub modified_ms: Option<i64>,
    /// UNIX time millis from `deleted`, when present. Non-none marks a
    /// tombstone commit.
    pub deleted_ms: Option<i64>,
    /// SHA from the `prev` blob, when present. Absent for new objects
    /// and redactions.
    pub prev: Option<String>,
}

impl ObjectHeader {
    pub fn is_tombstone(&self) -> bool {
        self.deleted_ms.is_some()
    }
}

/// The parents of one commit, split into the previous-version parent
/// (from the `prev` blob) and the link parents (each attributed to the
/// `.link` file that named it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedParents {
    /// SHA named by the `prev` blob, when present.
    pub prev: Option<String>,
    /// Link parents attributed to the `.link` file that named them.
    pub links: Vec<LinkParent>,
    /// Commit parents that appear in the commit but do not match `prev`
    /// or any listed `.link` file. Empty when everything checks out.
    pub unattributed: Vec<String>,
    /// SHAs named by `prev` or a `.link` file that are not commit
    /// parents. Empty when everything checks out.
    pub missing: Vec<String>,
}

/// One SHA named by one `.link` file, and the path of that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkParent {
    pub link_file: String,
    pub sha: String,
}

/// The full contents of one `.link` file discovered under a commit
/// tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkFile {
    /// Path relative to the commit tree root.
    pub path: String,
    /// SHAs listed in the file, in file order.
    pub shas: Vec<String>,
}

/// List every ref under `refs/gage/`, with its type bucket, id, and
/// tip SHA. Refs whose name does not have the shape
/// `refs/gage/<bucket>/<id>` (a single segment for bucket, at least one
/// segment for id) are skipped.
pub fn list_gage_refs() -> Result<Vec<ObjectRef>, StoreError> {
    list_gage_refs_at(&store_path())
}

pub fn list_gage_refs_at(path: &Path) -> Result<Vec<ObjectRef>, StoreError> {
    if !crate::exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }
    let out = run(git_in(
        path,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/gage/",
        ],
    ))?;
    let mut refs = Vec::new();
    for line in out.lines() {
        let (name, sha) = line
            .split_once(' ')
            .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
        let Some(rest) = name.strip_prefix("refs/gage/") else {
            continue;
        };
        let Some((bucket, id)) = rest.split_once('/') else {
            continue;
        };
        if bucket.is_empty() || id.is_empty() {
            continue;
        }
        refs.push(ObjectRef {
            ref_name: name.to_string(),
            type_bucket: bucket.to_string(),
            id: id.to_string(),
            tip_sha: sha.to_string(),
        });
    }
    Ok(refs)
}

/// Read a commit's object header (the four common files, `deleted`,
/// and `prev`). A missing `object` or `id` blob is a parse error; the
/// other files are optional.
pub fn read_header(commit: &str) -> Result<ObjectHeader, StoreError> {
    read_header_at(&store_path(), commit)
}

pub fn read_header_at(path: &Path, commit: &str) -> Result<ObjectHeader, StoreError> {
    let names = tree_names(path, commit)?;
    let object_blob = read_blob_string(path, &format!("{commit}:object"))?;
    let (object_type, version) = object_blob
        .trim()
        .split_once(' ')
        .map(|(t, v)| (t.to_string(), v.to_string()))
        .ok_or_else(|| StoreError::Parse(format!("object blob at {commit}: {object_blob:?}")))?;
    let id = read_blob_string(path, &format!("{commit}:id"))?
        .trim()
        .to_string();
    let created_ms = read_optional_ms(path, commit, "created", &names)?;
    let modified_ms = read_optional_ms(path, commit, "modified", &names)?;
    let deleted_ms = read_optional_ms(path, commit, "deleted", &names)?;
    let prev = if names.iter().any(|n| n == "prev") {
        Some(
            read_blob_string(path, &format!("{commit}:prev"))?
                .trim()
                .to_string(),
        )
    } else {
        None
    };
    Ok(ObjectHeader {
        object_type,
        version,
        id,
        created_ms,
        modified_ms,
        deleted_ms,
        prev,
    })
}

fn read_optional_ms(
    path: &Path,
    commit: &str,
    name: &str,
    tree_names: &[String],
) -> Result<Option<i64>, StoreError> {
    if !tree_names.iter().any(|n| n == name) {
        return Ok(None);
    }
    let text = read_blob_string(path, &format!("{commit}:{name}"))?;
    let ms: i64 = text
        .trim()
        .parse()
        .map_err(|e| StoreError::Parse(format!("{name} at {commit}: {e}")))?;
    Ok(Some(ms))
}

fn tree_names(path: &Path, commit: &str) -> Result<Vec<String>, StoreError> {
    let out = run(git_in(path, ["ls-tree", "--name-only", commit]))?;
    Ok(out.lines().map(String::from).collect())
}

fn read_blob_string(path: &Path, spec: &str) -> Result<String, StoreError> {
    run(git_in(path, ["cat-file", "-p", spec]))
}

/// Walk the commit tree recursively and return every `.link` file with
/// its listed SHAs. The path is relative to the commit's root.
pub fn find_link_files(commit: &str) -> Result<Vec<LinkFile>, StoreError> {
    find_link_files_at(&store_path(), commit)
}

pub fn find_link_files_at(path: &Path, commit: &str) -> Result<Vec<LinkFile>, StoreError> {
    let out = run(git_in(path, ["ls-tree", "-r", "--name-only", commit]))?;
    let mut files = Vec::new();
    for name in out.lines() {
        if !name.ends_with(".link") {
            continue;
        }
        let content = read_blob_string(path, &format!("{commit}:{name}"))?;
        let shas: Vec<String> = content
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        files.push(LinkFile {
            path: name.to_string(),
            shas,
        });
    }
    Ok(files)
}

/// Split a commit's parents into `prev`, link parents (attributed to
/// their `.link` file), and any residuals. `unattributed` and
/// `missing` are empty in a well-formed commit; the viewer surfaces
/// them as warnings so structural anomalies are visible.
pub fn classify_parents(commit: &str) -> Result<ClassifiedParents, StoreError> {
    classify_parents_at(&store_path(), commit)
}

pub fn classify_parents_at(path: &Path, commit: &str) -> Result<ClassifiedParents, StoreError> {
    let meta = read_commit_at(path, commit)?;
    let header = read_header_at(path, commit)?;
    let link_files = find_link_files_at(path, commit)?;

    let mut links: Vec<LinkParent> = Vec::new();
    for file in &link_files {
        for sha in &file.shas {
            links.push(LinkParent {
                link_file: file.path.clone(),
                sha: sha.clone(),
            });
        }
    }

    let expected: Vec<String> = header
        .prev
        .iter()
        .cloned()
        .chain(links.iter().map(|l| l.sha.clone()))
        .collect();
    let unattributed: Vec<String> = meta
        .parents
        .iter()
        .filter(|p| !expected.iter().any(|e| e == *p))
        .cloned()
        .collect();
    let missing: Vec<String> = expected
        .iter()
        .filter(|e| !meta.parents.iter().any(|p| p == *e))
        .cloned()
        .collect();

    Ok(ClassifiedParents {
        prev: header.prev,
        links,
        unattributed,
        missing,
    })
}

/// Walk the `prev` chain from `commit`, oldest last (index 0 is
/// `commit` itself). Stops at the first commit whose header has no
/// `prev` blob.
pub fn walk_prev_chain(commit: &str) -> Result<Vec<String>, StoreError> {
    walk_prev_chain_at(&store_path(), commit)
}

pub fn walk_prev_chain_at(path: &Path, commit: &str) -> Result<Vec<String>, StoreError> {
    let mut chain = Vec::new();
    let mut current = commit.to_string();
    loop {
        chain.push(current.clone());
        let header = read_header_at(path, &current)?;
        match header.prev {
            Some(prev) if !prev.is_empty() => current = prev,
            _ => return Ok(chain),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::{NoteInput, note_add_at, note_edit_at};
    use crate::{dataset_add_at, init_at};

    fn init_store(dir: &Path) -> std::path::PathBuf {
        let store = dir.join("store.git");
        init_at(&store).unwrap();
        store
    }

    #[test]
    fn list_gage_refs_returns_note_and_dataset_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let note_id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let dataset_id = dataset_add_at(&store).unwrap();

        let refs = list_gage_refs_at(&store).unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.ref_name.as_str()).collect();
        assert!(names.contains(&format!("refs/gage/notes/{note_id}").as_str()));
        assert!(names.contains(&format!("refs/gage/datasets/{dataset_id}").as_str()));

        let note_row = refs
            .iter()
            .find(|r| r.id == note_id)
            .expect("note ref present");
        assert_eq!(note_row.type_bucket, "notes");
    }

    #[test]
    fn read_header_reports_type_and_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();

        let header = read_header_at(&store, &format!("refs/gage/notes/{id}")).unwrap();
        assert_eq!(header.object_type, "gage::note");
        assert_eq!(header.version, "1");
        assert_eq!(header.id, id);
        assert!(!header.is_tombstone());
        assert!(header.prev.is_none());
        assert!(header.created_ms.is_some());
        assert_eq!(header.created_ms, header.modified_ms);
    }

    #[test]
    fn read_header_after_edit_reports_prev() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let ref_path = format!("refs/gage/notes/{id}");
        let first = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();
        std::thread::sleep(std::time::Duration::from_millis(2));
        note_edit_at(&store, &id, "v2").unwrap();

        let header = read_header_at(&store, &ref_path).unwrap();
        assert_eq!(header.prev.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn classify_parents_splits_prev_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let root = note_add_at(
            &store,
            NoteInput {
                name: "root",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let root_ref = format!("refs/gage/notes/{root}");
        let root_commit = run(git_in(&store, ["rev-parse", &root_ref]))
            .unwrap()
            .trim()
            .to_string();
        let child = note_add_at(
            &store,
            NoteInput {
                name: "reply",
                value: "first",
                author: "user:test",
                targets: &[format!("note:{root}")],
            },
        )
        .unwrap();
        let child_ref = format!("refs/gage/notes/{child}");
        let first_child = run(git_in(&store, ["rev-parse", &child_ref]))
            .unwrap()
            .trim()
            .to_string();
        note_edit_at(&store, &child, "second").unwrap();

        let classified = classify_parents_at(&store, &child_ref).unwrap();
        assert_eq!(classified.prev.as_deref(), Some(first_child.as_str()));
        assert_eq!(classified.links.len(), 1);
        assert_eq!(classified.links[0].link_file, "target.link");
        assert_eq!(classified.links[0].sha, root_commit);
        assert!(classified.unattributed.is_empty());
        assert!(classified.missing.is_empty());
    }

    #[test]
    fn walk_prev_chain_returns_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let id = note_add_at(
            &store,
            NoteInput {
                name: "n",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let ref_path = format!("refs/gage/notes/{id}");
        std::thread::sleep(std::time::Duration::from_millis(2));
        note_edit_at(&store, &id, "v2").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        note_edit_at(&store, &id, "v3").unwrap();

        let tip = run(git_in(&store, ["rev-parse", &ref_path]))
            .unwrap()
            .trim()
            .to_string();
        let chain = walk_prev_chain_at(&store, &tip).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], tip);
    }

    #[test]
    fn find_link_files_reads_target_link() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let root = note_add_at(
            &store,
            NoteInput {
                name: "root",
                value: "v",
                author: "user:test",
                targets: &[],
            },
        )
        .unwrap();
        let root_commit = run(git_in(
            &store,
            ["rev-parse", &format!("refs/gage/notes/{root}")],
        ))
        .unwrap()
        .trim()
        .to_string();
        let child = note_add_at(
            &store,
            NoteInput {
                name: "reply",
                value: "v",
                author: "user:test",
                targets: &[format!("note:{root}")],
            },
        )
        .unwrap();
        let files = find_link_files_at(&store, &format!("refs/gage/notes/{child}")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "target.link");
        assert_eq!(files[0].shas, vec![root_commit]);
    }
}
