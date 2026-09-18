//! Session objects: `gage::session 1`.
//!
//! Content is `attrs.json` (driver, native session id, session type,
//! optional content format) and the opaque `files/**` subtree holding
//! the session content as provided by the driver. The object id is
//! derived from `(driver_name, native_session_id)`, so the same native
//! session maps to the same object over time. Adding a session whose
//! content matches the stored version writes nothing; changed content
//! writes an edit commit. Tree construction, commit parents, and
//! edits are the generic object model's job; see [`crate::object`].

use std::collections::BTreeMap;
use std::path::Path;

use gage_core::uuid::derive_id;
use gage_session::{SessionType, SourceSession};
use serde::{Deserialize, Serialize};

use crate::git::{git_in, run};
use crate::object::{EditOutcome, ObjectTree, object_ref, read_object_at, require_type};
use crate::writer::{mktree, write_blob_stream};
use crate::{StoreError, exists, object};

const OBJECT_TYPE: &str = "gage::session";
const OBJECT_VERSION: &str = "1";
const FILES_TREE: &str = "files";

/// The `attrs.json` shape of a session.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionAttrs {
    /// `"<driver name> <driver version>"`.
    pub driver: String,
    /// Native session id, as the driver reported it.
    pub session_id: String,
    /// `"<type name> <type version>"`, e.g. `"claude 1"`.
    pub session_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_format: Option<String>,
}

/// Outcome of writing one session to the store.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionAddOutcome {
    /// Derived Gage object id for the session.
    pub id: String,
    /// Commit SHA of the resulting version. When `outcome` is
    /// [`SessionOutcome::Unchanged`] this is the existing commit.
    pub commit_sha: String,
    pub outcome: SessionOutcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    /// New session object created.
    Added,
    /// Existing session object updated with new content.
    Updated,
    /// Existing session content matched; no commit was written.
    Unchanged,
}

/// Derive the Gage object id of a session from its driver name and
/// native session id.
pub fn session_object_id(driver_name: &str, native_session_id: &str) -> String {
    derive_id(&format!("session\0{driver_name}\0{native_session_id}"))
}

/// Write `reader`'s content as a session object. Idempotent when the
/// native session's files and attrs are unchanged.
pub fn session_add_at(
    path: &Path,
    driver_name: &str,
    driver_version: &str,
    reader: &mut dyn SourceSession,
) -> Result<SessionAddOutcome, StoreError> {
    if !exists(path) {
        return Err(StoreError::NotFound(path.to_path_buf()));
    }

    let native_session_id = reader.session_id().to_string();
    let id = session_object_id(driver_name, &native_session_id);
    let attrs = SessionAttrs {
        driver: format!("{driver_name} {driver_version}"),
        session_id: native_session_id,
        session_type: reader.session_type().to_string(),
        content_format: reader.content_format().map(str::to_string),
    };

    let mut file_entries: Vec<(String, String)> = Vec::new();
    for file in reader.files() {
        let file = file.map_err(|e| StoreError::Parse(format!("driver: {e}")))?;
        let sha = write_blob_stream(path, file.content)?;
        file_entries.push((file.path, sha));
    }
    let files_tree_sha = build_files_tree(path, file_entries)?;

    let mut tree = ObjectTree {
        attrs: Some(
            serde_json::to_value(&attrs)
                .map_err(|e| StoreError::Parse(format!("session attrs encode: {e}")))?,
        ),
        ..ObjectTree::default()
    };
    tree.subtrees.insert(FILES_TREE.to_string(), files_tree_sha);

    let existing = match run(git_in(path, ["rev-parse", "--verify", &object_ref(&id)])) {
        Ok(sha) => Some(read_object_at(path, sha.trim())?),
        Err(StoreError::Git { .. }) => None,
        Err(e) => return Err(e),
    };
    let subject = format!("{driver_name}:{}", attrs.session_id);
    match existing {
        None => {
            let message = format!("session: {subject}");
            let commit_sha =
                object::create(path, OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
            Ok(SessionAddOutcome {
                id,
                commit_sha,
                outcome: SessionOutcome::Added,
            })
        }
        Some(current) => {
            require_type(&current, OBJECT_TYPE)?;
            let message = format!("session edit: {subject}");
            match object::edit(path, &current, &tree, &message)? {
                EditOutcome::Unchanged => Ok(SessionAddOutcome {
                    id,
                    commit_sha: current.commit_sha,
                    outcome: SessionOutcome::Unchanged,
                }),
                EditOutcome::Written(commit_sha) => Ok(SessionAddOutcome {
                    id,
                    commit_sha,
                    outcome: SessionOutcome::Updated,
                }),
            }
        }
    }
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
    mktree(path, &lines)
}

/// A stored session presented for reading, resolved to its commit.
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
    let object = read_object_at(path, commit_sha)?;
    require_type(&object, OBJECT_TYPE)?;
    let attrs_value = object
        .tree
        .attrs
        .ok_or_else(|| StoreError::Parse(format!("session {commit_sha}: missing attrs.json")))?;
    let attrs: SessionAttrs = serde_json::from_value(attrs_value)
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
        id: object.header.id,
        commit_sha: object.commit_sha,
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
        ["ls-tree", "-r", "-l", &format!("{commit_sha}:{FILES_TREE}")],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_at;
    use gage_session::{DriverError, SessionFile};
    use std::io::Cursor;

    fn init_store(dir: &Path) -> std::path::PathBuf {
        let store = dir.join("store.git");
        init_at(&store).unwrap();
        store
    }

    /// A native session with fixed content, for exercising the writer.
    struct FakeSession {
        id: String,
        files: Vec<(String, Vec<u8>)>,
        session_type: SessionType,
    }

    impl SourceSession for FakeSession {
        fn session_id(&self) -> &str {
            &self.id
        }

        fn session_type(&self) -> &SessionType {
            &self.session_type
        }

        fn content_format(&self) -> Option<&str> {
            None
        }

        fn files(&mut self) -> Box<dyn Iterator<Item = Result<SessionFile, DriverError>> + '_> {
            Box::new(self.files.iter().map(|(path, bytes)| {
                Ok(SessionFile {
                    path: path.clone(),
                    content: Box::new(Cursor::new(bytes.clone())),
                })
            }))
        }
    }

    fn fake(id: &str, files: &[(&str, &str)]) -> FakeSession {
        FakeSession {
            id: id.to_string(),
            files: files
                .iter()
                .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
                .collect(),
            session_type: SessionType::new("fake", "1"),
        }
    }

    fn cat(store: &Path, spec: &str) -> String {
        run(git_in(store, ["cat-file", "-p", spec])).unwrap()
    }

    #[test]
    fn add_writes_session_object_with_files_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let mut session = fake("s1", &[("session.jsonl", "{}\n"), ("sub/a.txt", "a")]);

        let outcome = session_add_at(&store, "fake", "0.1", &mut session).unwrap();
        assert_eq!(outcome.outcome, SessionOutcome::Added);
        assert_eq!(outcome.id, session_object_id("fake", "s1"));

        let ref_path = object_ref(&outcome.id);
        let listing = run(git_in(&store, ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["attrs.json", "created", "files", "id", "modified", "type"]
        );
        assert_eq!(
            cat(&store, &format!("{ref_path}:type")),
            "gage::session 1\n"
        );
        assert_eq!(
            cat(&store, &format!("{ref_path}:attrs.json")),
            "{\"driver\":\"fake 0.1\",\"session_id\":\"s1\",\"session_type\":\"fake 1\"}\n"
        );
        assert_eq!(cat(&store, &format!("{ref_path}:files/sub/a.txt")), "a");

        let record = session_at_commit(&store, &outcome.commit_sha).unwrap();
        assert_eq!(record.id, outcome.id);
        assert_eq!(record.driver_name, "fake");
        assert_eq!(record.driver_version, "0.1");
        assert_eq!(record.session_type, SessionType::new("fake", "1"));
        assert_eq!(record.attrs.session_id, "s1");
        assert_eq!(record.size, 4);
    }

    #[test]
    fn add_is_idempotent_and_updates_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());

        let first = session_add_at(
            &store,
            "fake",
            "0.1",
            &mut fake("s1", &[("session.jsonl", "{}\n")]),
        )
        .unwrap();
        let again = session_add_at(
            &store,
            "fake",
            "0.1",
            &mut fake("s1", &[("session.jsonl", "{}\n")]),
        )
        .unwrap();
        assert_eq!(again.outcome, SessionOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);

        let grown = session_add_at(
            &store,
            "fake",
            "0.1",
            &mut fake("s1", &[("session.jsonl", "{}\n{}\n")]),
        )
        .unwrap();
        assert_eq!(grown.outcome, SessionOutcome::Updated);
        assert_eq!(grown.id, first.id);
        assert_ne!(grown.commit_sha, first.commit_sha);
        assert_eq!(
            cat(&store, &format!("{}:parent", grown.commit_sha)),
            format!("{}\n", first.commit_sha)
        );
    }

    #[test]
    fn session_at_commit_rejects_other_types() {
        let tmp = tempfile::tempdir().unwrap();
        let store = init_store(tmp.path());
        let dataset = crate::dataset_new_at(&store).unwrap();
        let sha = run(git_in(&store, ["rev-parse", &object_ref(&dataset)]))
            .unwrap()
            .trim()
            .to_string();
        assert!(matches!(
            session_at_commit(&store, &sha).unwrap_err(),
            StoreError::WrongType { actual, .. } if actual == "gage::dataset"
        ));
    }
}
