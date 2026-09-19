//! Session objects: `gage::session 1`, reached through [`SessionStore`].
//!
//! Content is `attrs.json` (driver, native session id, session type,
//! optional content format) and the opaque `files/**` subtree holding
//! the session content as provided by the driver. The object id is
//! derived from `(driver_name, native_session_id)`, so the same native
//! session maps to the same object over time. Adding a session whose
//! content matches the stored version writes nothing; changed content
//! writes an edit commit. Tree construction, commit parents, and
//! edits are the generic object model's job; see [`crate::object`].

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use gage_core::uuid::derive_id;
use gage_session::{SessionSummary, SessionType, SourceSession};
use serde::{Deserialize, Serialize};

use crate::index::{ObjectQuery, Order};
use crate::object::{EditOutcome, Object, ObjectTree, object_ref, require_type};
use crate::writer::{TreeInput, is_dot_git, mktree, write_blob_stream};
use crate::{Store, StoreError};

pub(crate) const OBJECT_TYPE: &str = "gage::session";
const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from a session's `attrs.json`.
/// `summary` is a driver projection; until a driver writes it the
/// `model` filter selects nothing.
pub(crate) const INDEXED_ATTRS: &[&str] = &["summary.model"];
const FILES_TREE: &str = "files";

/// Session operations over an opened store.
pub struct SessionStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for SessionStore<'a> {
    fn from(store: &'a Store) -> Self {
        SessionStore { store }
    }
}

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
    /// The driver's projection of the session, written at add time.
    /// Absent when the driver reported nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummaryAttrs>,
}

/// `attrs.summary`: the driver's [`SessionSummary`] as stored. Only
/// the driver can compute these; the store copies them verbatim.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct SummaryAttrs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl From<SessionSummary> for SummaryAttrs {
    fn from(s: SessionSummary) -> Self {
        SummaryAttrs {
            title: s.title,
            model: s.model,
            message_count: s.message_count,
            size: s.size,
        }
    }
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

/// A stored session presented for reading, resolved to its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub commit_sha: String,
    pub attrs: SessionAttrs,
    pub session_type: SessionType,
    pub driver_name: String,
    pub driver_version: String,
    /// The driver's size of the session's own files, from
    /// `attrs.summary`. `None` when the driver did not report one.
    pub size: Option<u64>,
}

/// Derive the Gage object id of a session from its driver name and
/// native session id.
pub fn session_object_id(driver_name: &str, native_session_id: &str) -> String {
    derive_id(&format!("session\0{driver_name}\0{native_session_id}"))
}

impl SessionStore<'_> {
    /// Write `reader`'s content as a session object. Idempotent when
    /// the native session's files and attrs are unchanged.
    pub fn add(
        &self,
        driver_name: &str,
        driver_version: &str,
        reader: &mut dyn SourceSession,
    ) -> Result<SessionAddOutcome, StoreError> {
        let path = self.store.path();
        let native_session_id = reader.session_id().to_string();
        let id = session_object_id(driver_name, &native_session_id);
        let attrs = SessionAttrs {
            driver: format!("{driver_name} {driver_version}"),
            session_id: native_session_id,
            session_type: reader.session_type().to_string(),
            content_format: reader.content_format().map(str::to_string),
            summary: {
                let summary = reader.summary();
                (summary != SessionSummary::default()).then(|| SummaryAttrs::from(summary))
            },
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

        let subject = format!("{driver_name}:{}", attrs.session_id);
        match self.store.rev_parse(&object_ref(&id))? {
            None => {
                let message = format!("session: {subject}");
                let commit_sha =
                    self.store
                        .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
                Ok(SessionAddOutcome {
                    id,
                    commit_sha,
                    outcome: SessionOutcome::Added,
                })
            }
            Some(sha) => {
                let current = self.store.read_object(&sha)?;
                require_type(&current, OBJECT_TYPE)?;
                let message = format!("session edit: {subject}");
                match self.store.edit(&current, &tree, &message)? {
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

    /// Every live session, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<SessionRecord, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over sessions.
    pub fn query(&self) -> SessionQuery<'_> {
        SessionQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// Read the session at the given commit SHA.
    pub fn at_commit(&self, commit_sha: &str) -> Result<SessionRecord, StoreError> {
        let object = self.store.read_object(commit_sha)?;
        decode(object)
    }
}

/// A selection over sessions: filters on the indexed attributes, an
/// order, and a limit. `iter` reads matching sessions one at a time.
pub struct SessionQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> SessionQuery<'a> {
    /// Select sessions whose `summary.model` equals `model`.
    pub fn model(mut self, model: &str) -> Self {
        self.query.attrs.push(("summary.model", model.to_string()));
        self
    }

    pub fn order(mut self, order: Order) -> Self {
        self.query.order = order;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.query.limit = Some(limit);
        self
    }

    /// Run the selection. Matching tips are resolved by the index in
    /// one step; each session is read from the repository as the
    /// iterator advances.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<SessionRecord, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let shas = store.select(&self.query)?;
        Ok(shas
            .into_iter()
            .map(move |sha| decode(store.read_object(&sha)?)))
    }
}

fn decode(object: Object) -> Result<SessionRecord, StoreError> {
    let commit_sha = object.commit_sha.as_str();
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
    let size = attrs.summary.as_ref().and_then(|s| s.size);
    Ok(SessionRecord {
        id: object.header.id,
        commit_sha: object.commit_sha.clone(),
        attrs,
        session_type,
        driver_name,
        driver_version,
        size,
    })
}

/// Build the `files/` tree from `(relative_path, blob_sha)` pairs.
/// Validates the full path set once, then recurses on subdirectories.
fn build_files_tree(path: &Path, entries: Vec<(String, String)>) -> Result<String, StoreError> {
    validate_session_paths(&entries)?;
    build_files_tree_inner(path, entries)
}

/// Checks the driver's file paths against Gage's tree rules. Runs once
/// at the top-level call so the full offending path is available in
/// every error.
fn validate_session_paths(entries: &[(String, String)]) -> Result<(), StoreError> {
    let mut seen: HashSet<&str> = HashSet::new();
    for (rel, _) in entries {
        validate_session_path(rel)?;
        if !seen.insert(rel.as_str()) {
            return Err(StoreError::InvalidPath {
                path: rel.clone(),
                reason: "duplicate session file path".to_string(),
            });
        }
    }
    let prefixes: HashSet<&str> = entries
        .iter()
        .flat_map(|(p, _)| directory_prefixes(p))
        .collect();
    for (p, _) in entries {
        if prefixes.contains(p.as_str()) {
            return Err(StoreError::InvalidPath {
                path: p.clone(),
                reason: "file path is also a directory prefix of another path".to_string(),
            });
        }
    }
    Ok(())
}

/// Rejects malformed paths. Per-component checks mirror
/// [`crate::writer::validate_tree_name`]; the whole-path checks catch
/// shapes that would otherwise split into empty components.
fn validate_session_path(rel: &str) -> Result<(), StoreError> {
    let invalid = |reason: &str| StoreError::InvalidPath {
        path: rel.to_string(),
        reason: reason.to_string(),
    };
    if rel.is_empty() {
        return Err(invalid("empty path"));
    }
    if rel.starts_with('/') {
        return Err(invalid("leading `/`"));
    }
    if rel.ends_with('/') {
        return Err(invalid("trailing `/`"));
    }
    for component in rel.split('/') {
        if component.is_empty() {
            return Err(invalid("empty component (`//` in path)"));
        }
        if component.contains('\0') {
            return Err(invalid("component contains NUL"));
        }
        if component == "." || component == ".." {
            return Err(invalid("component is `.` or `..`"));
        }
        if is_dot_git(component) {
            return Err(invalid("component is `.git`"));
        }
    }
    Ok(())
}

/// Every proper directory prefix of `path`. `"a/b/c"` yields `"a"`
/// and `"a/b"`; a path with no `/` yields nothing.
fn directory_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(i, _)| &path[..i])
}

fn build_files_tree_inner(
    path: &Path,
    entries: Vec<(String, String)>,
) -> Result<String, StoreError> {
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
    let mut subtree_shas: Vec<(String, String)> = Vec::with_capacity(subdirs.len());
    for (dir, sub) in subdirs {
        subtree_shas.push((dir, build_files_tree_inner(path, sub)?));
    }
    let mut entries: Vec<TreeInput<'_>> = blobs
        .iter()
        .map(|(name, sha)| TreeInput {
            mode: "100644",
            sha,
            name,
        })
        .collect();
    entries.extend(subtree_shas.iter().map(|(dir, sha)| TreeInput {
        mode: "040000",
        sha,
        name: dir,
    }));
    mktree(path, &entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DatasetStore;
    use crate::git::{git_in, run};
    use crate::test_support::open_store;
    use gage_session::{DriverError, SessionFile};
    use std::io::Cursor;

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

        fn summary(&self) -> SessionSummary {
            SessionSummary {
                size: Some(self.files.iter().map(|(_, b)| b.len() as u64).sum()),
                ..SessionSummary::default()
            }
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

    fn cat(store: &Store, spec: &str) -> String {
        run(git_in(store.path(), ["cat-file", "-p", spec])).unwrap()
    }

    #[test]
    fn add_writes_session_object_with_files_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let mut session = fake("s1", &[("session.jsonl", "{}\n"), ("sub/a.txt", "a")]);

        let outcome = sessions.add("fake", "0.1", &mut session).unwrap();
        assert_eq!(outcome.outcome, SessionOutcome::Added);
        assert_eq!(outcome.id, session_object_id("fake", "s1"));

        let ref_path = object_ref(&outcome.id);
        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
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
            "{\"driver\":\"fake 0.1\",\"session_id\":\"s1\",\"session_type\":\"fake 1\",\"summary\":{\"size\":4}}\n"
        );
        assert_eq!(cat(&store, &format!("{ref_path}:files/sub/a.txt")), "a");

        let record = sessions.at_commit(&outcome.commit_sha).unwrap();
        assert_eq!(record.id, outcome.id);
        assert_eq!(record.driver_name, "fake");
        assert_eq!(record.driver_version, "0.1");
        assert_eq!(record.session_type, SessionType::new("fake", "1"));
        assert_eq!(record.attrs.session_id, "s1");
        assert_eq!(record.size, Some(4));
    }

    #[test]
    fn add_is_idempotent_and_updates_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add("fake", "0.1", &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let again = sessions
            .add("fake", "0.1", &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        assert_eq!(again.outcome, SessionOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);

        let grown = sessions
            .add(
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
    fn at_commit_rejects_other_types() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let dataset = DatasetStore::from(&store).create().unwrap();
        let sha = store.rev_parse(&object_ref(&dataset)).unwrap().unwrap();
        assert!(matches!(
            SessionStore::from(&store).at_commit(&sha).unwrap_err(),
            StoreError::WrongType { actual, .. } if actual == "gage::dataset"
        ));
    }

    #[test]
    fn add_accepts_legal_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let mut session = fake(
            "nested",
            &[("a/b/c.jsonl", "1"), ("a/b/d.jsonl", "2"), ("a/e.txt", "3")],
        );
        let outcome = sessions.add("fake", "0.1", &mut session).unwrap();
        assert_eq!(outcome.outcome, SessionOutcome::Added);
        let ref_path = object_ref(&outcome.id);
        let listing = run(git_in(
            store.path(),
            ["ls-tree", "-r", "--name-only", &format!("{ref_path}:files")],
        ))
        .unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["a/b/c.jsonl", "a/b/d.jsonl", "a/e.txt"]
        );
    }

    #[test]
    fn add_rejects_invalid_session_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let cases: &[(&[(&str, &str)], &str, &str)] = &[
            (&[("", "x")], "", "empty path"),
            (&[("/a", "x")], "/a", "leading `/`"),
            (&[("a/", "x")], "a/", "trailing `/`"),
            (&[("a//b", "x")], "a//b", "empty component (`//` in path)"),
            (&[("a/./b", "x")], "a/./b", "component is `.` or `..`"),
            (&[("a/../b", "x")], "a/../b", "component is `.` or `..`"),
            (
                &[(".git/config", "x")],
                ".git/config",
                "component is `.git`",
            ),
            (
                &[(".GIT/config", "x")],
                ".GIT/config",
                "component is `.git`",
            ),
            (&[("a/git~1", "x")], "a/git~1", "component is `.git`"),
            (&[("a\0b", "x")], "a\0b", "component contains NUL"),
            (
                &[("dup", "x"), ("dup", "y")],
                "dup",
                "duplicate session file path",
            ),
            (
                &[("a", "x"), ("a/b", "y")],
                "a",
                "file path is also a directory prefix of another path",
            ),
        ];
        for (i, (files, expected_path, expected_reason)) in cases.iter().enumerate() {
            let mut session = fake(&format!("s{i}"), files);
            let err = sessions.add("fake", "0.1", &mut session).unwrap_err();
            match err {
                StoreError::InvalidPath { path, reason } => {
                    assert_eq!(path, *expected_path, "case {i}: {files:?}");
                    assert_eq!(reason, *expected_reason, "case {i}: {files:?}");
                }
                other => panic!("case {i} {files:?}: expected InvalidPath, got {other:?}"),
            }
        }
    }
}
