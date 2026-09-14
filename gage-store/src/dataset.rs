//! Dataset writer and reader.
//!
//! A dataset is a ref under `refs/gage/datasets/<id>`. Its tree carries
//! `format` (`gage-dataset 1\n`), `created`, and `modified`, and any
//! session directories added later. An `add` commit is parentless.

use std::path::Path;

use gage_core::datetime::now_ms;
use gage_core::uuid::new_uuid;

use crate::writer::{commit_tree, mktree, write_blob};
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
