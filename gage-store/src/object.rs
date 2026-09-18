//! The Gage object model: one tree grammar and one create, edit, and
//! delete path for every object type.
//!
//! An object is a commit under `refs/gage/object/<id>`. Its tree
//! carries the marker files `type` (`gage::<name> <version>`), `id`,
//! `created`, `modified`, and, on a tombstone, `deleted`; an edit adds
//! `parent` holding the previous version's SHA. Type-specific content
//! is `attrs.json`, blob-valued attribute files (`value.txt`), link
//! files (`*.link`, one commit SHA per line), and opaque subtrees
//! (`files/`). See README: Appendix: Object tree layout.
//!
//! The rules that make a commit well formed live here and nowhere
//! else: the `parent` SHA is the first commit parent, every SHA in
//! every link file is a commit parent, a tombstone is parentless and
//! carries only the markers, and an edit whose content matches the
//! current commit writes nothing. Type modules (`note`, `dataset`,
//! `session`) supply an [`ObjectTree`] and decode one; they do not
//! build tree entries.
//!
//! The Git primitives these operations compose from live in
//! [`crate::git`] and [`crate::writer`].

use std::collections::BTreeMap;
use std::path::Path;

use gage_core::datetime::now_ms;
use serde_json::Value as JsonValue;

use crate::git::{git_in, run};
use crate::writer::{commit_tree, mktree, write_blob};
use crate::{Store, StoreError};

/// Full ref name of the object with the given id.
pub(crate) fn object_ref(id: &str) -> String {
    format!("refs/gage/object/{id}")
}

/// One ref under `refs/gage/object/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    /// Full ref name, e.g. `refs/gage/object/<id>`.
    pub ref_name: String,
    pub id: String,
    /// SHA the ref points to.
    pub tip_sha: String,
}

/// The marker files of one commit. `type` and `id` are required; the
/// others are optional so a reader can classify a commit without
/// parsing its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectHeader {
    /// The `<name>` half of the `type` blob (`gage::note` etc.).
    pub object_type: String,
    /// The `<version>` half of the `type` blob (`1` etc.).
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
    /// SHA from the `parent` blob, when present. Absent for new objects
    /// and redactions.
    pub parent: Option<String>,
}

impl ObjectHeader {
    pub fn is_tombstone(&self) -> bool {
        self.deleted_ms.is_some()
    }
}

/// The type-specific content of an object, independent of the type.
/// A type module builds one of these to write and decodes one when
/// reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectTree {
    /// Structured state, written to `attrs.json`. `None` when the type
    /// has no structured state.
    pub attrs: Option<JsonValue>,
    /// Blob-valued attribute files by file name (`value.txt`).
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// Link files by file name (`target.link`), each listing commit
    /// SHAs in file order. Every SHA becomes a commit parent.
    pub links: BTreeMap<String, Vec<String>>,
    /// Opaque subtrees by directory name (`files`), each a tree SHA
    /// built ahead of time by the type module.
    pub subtrees: BTreeMap<String, String>,
}

/// One object at one commit: the markers, the content, and the raw
/// entry SHAs the content was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub commit_sha: String,
    pub header: ObjectHeader,
    pub tree: ObjectTree,
    /// Top-level tree entries by name, for reuse on edit and delete.
    entries: BTreeMap<String, TreeEntryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntryRef {
    mode: String,
    kind: String,
    sha: String,
}

const MARKERS: [&str; 6] = ["type", "id", "created", "modified", "deleted", "parent"];

/// Outcome of [`Store::edit`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditOutcome {
    /// The candidate content matched the current commit; nothing was
    /// written.
    Unchanged,
    /// A child commit was written with the given SHA.
    Written(String),
}

impl Store {
    /// Resolve a full id or unique prefix to `(id, tip_sha)`. The
    /// namespace is flat, so a prefix matches objects of every type;
    /// callers that need a type decode the object and check.
    pub fn resolve_id(&self, id_or_prefix: &str) -> Result<(String, String), StoreError> {
        let pattern = format!("{}*", object_ref(id_or_prefix));
        let matches = run(git_in(
            self.path(),
            [
                "for-each-ref",
                "--format=%(refname:strip=3) %(objectname)",
                &pattern,
            ],
        ))?;
        let lines: Vec<&str> = matches.lines().collect();
        match lines.as_slice() {
            [] => Err(StoreError::ObjectNotFound(id_or_prefix.to_string())),
            [only] => {
                let (id, sha) = only
                    .split_once(' ')
                    .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {only}")))?;
                Ok((id.to_string(), sha.to_string()))
            }
            many => Err(StoreError::AmbiguousId(
                id_or_prefix.to_string(),
                many.len(),
            )),
        }
    }

    /// Read the object at `commit` (any commit-ish): markers,
    /// `attrs.json`, every blob attribute, every link file, and the
    /// SHA of every subtree. A missing `type` or `id` blob is a parse
    /// error.
    pub fn read_object(&self, commit: &str) -> Result<Object, StoreError> {
        let commit_sha = run(git_in(
            self.path(),
            ["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        ))?
        .trim()
        .to_string();
        let entries = read_entries(self.path(), &commit_sha)?;
        let header = self.header_from_entries(&commit_sha, &entries)?;

        let mut tree = ObjectTree::default();
        for (name, entry) in &entries {
            if MARKERS.contains(&name.as_str()) {
                continue;
            }
            match entry.kind.as_str() {
                "tree" => {
                    tree.subtrees.insert(name.clone(), entry.sha.clone());
                }
                "blob" if name == "attrs.json" => {
                    let bytes = self.read_blob_bytes(&entry.sha)?;
                    let value: JsonValue = serde_json::from_slice(&bytes).map_err(|e| {
                        StoreError::Parse(format!("attrs.json at {commit_sha}: {e}"))
                    })?;
                    tree.attrs = Some(value);
                }
                "blob" if name.ends_with(".link") => {
                    let bytes = self.read_blob_bytes(&entry.sha)?;
                    tree.links.insert(name.clone(), parse_link(&bytes));
                }
                "blob" => {
                    tree.blobs
                        .insert(name.clone(), self.read_blob_bytes(&entry.sha)?);
                }
                other => {
                    return Err(StoreError::Parse(format!(
                        "unexpected entry kind {other} for {name} at {commit_sha}"
                    )));
                }
            }
        }
        Ok(Object {
            commit_sha,
            header,
            tree,
            entries,
        })
    }

    /// Resolve `id_or_prefix` to its current commit, verified to be a
    /// live object of type `object_type`.
    pub(crate) fn resolve_typed(
        &self,
        id_or_prefix: &str,
        object_type: &str,
    ) -> Result<Object, StoreError> {
        let (id, sha) = self.resolve_id(id_or_prefix)?;
        let object = self.read_object(&sha)?;
        require_type(&object, object_type)?;
        if object.header.is_tombstone() {
            return Err(StoreError::ObjectDeleted(id));
        }
        Ok(object)
    }

    /// Write a new object: a parentless commit (except for link
    /// parents) under `refs/gage/object/<id>`, which must not exist.
    /// Returns the commit SHA.
    pub(crate) fn create(
        &self,
        object_type: &str,
        version: &str,
        id: &str,
        tree: &ObjectTree,
        message: &str,
    ) -> Result<String, StoreError> {
        let path = self.path();
        let now = now_ms();
        let type_sha = write_blob(path, format!("{object_type} {version}\n").as_bytes())?;
        let id_sha = write_blob(path, format!("{id}\n").as_bytes())?;
        let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;

        let mut entries = vec![
            blob_entry(&type_sha, "type"),
            blob_entry(&id_sha, "id"),
            blob_entry(&stamp_sha, "created"),
            blob_entry(&stamp_sha, "modified"),
        ];
        let content = write_content(path, tree)?;
        entries.extend(content.entries);
        let tree_sha = mktree(path, &entries)?;

        let parents: Vec<&str> = content.link_parents.iter().map(String::as_str).collect();
        let commit_sha = commit_tree(path, &tree_sha, message, &parents)?;
        run(git_in(
            path,
            ["update-ref", &object_ref(id), &commit_sha, ""],
        ))?;
        self.index_commit(&commit_sha)?;
        self.index.set_tip(id, Some(&commit_sha))?;
        Ok(commit_sha)
    }

    /// Write a new version of `current` with `tree` as its content.
    /// The child commit carries `parent` (the current SHA, first
    /// commit parent), reuses `type`, `id`, and `created`, bumps
    /// `modified`, and lists every link SHA as a parent. Returns
    /// [`EditOutcome::Unchanged`] without writing when the content
    /// matches the current commit entry for entry.
    pub(crate) fn edit(
        &self,
        current: &Object,
        tree: &ObjectTree,
        message: &str,
    ) -> Result<EditOutcome, StoreError> {
        if current.header.is_tombstone() {
            return Err(StoreError::ObjectDeleted(current.header.id.clone()));
        }
        let path = self.path();
        let content = write_content(path, tree)?;
        if content.shas == content_shas(&current.entries) {
            return Ok(EditOutcome::Unchanged);
        }

        let now = now_ms();
        let modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;
        let parent_sha = write_blob(path, format!("{}\n", current.commit_sha).as_bytes())?;
        let mut entries = vec![
            blob_entry(&current.marker_sha("type")?, "type"),
            blob_entry(&current.marker_sha("id")?, "id"),
            blob_entry(&current.marker_sha("created")?, "created"),
            blob_entry(&modified_sha, "modified"),
            blob_entry(&parent_sha, "parent"),
        ];
        entries.extend(content.entries);
        let tree_sha = mktree(path, &entries)?;

        let mut parents: Vec<&str> = vec![&current.commit_sha];
        parents.extend(content.link_parents.iter().map(String::as_str));
        let commit_sha = commit_tree(path, &tree_sha, message, &parents)?;
        run(git_in(
            path,
            [
                "update-ref",
                &object_ref(&current.header.id),
                &commit_sha,
                &current.commit_sha,
            ],
        ))?;
        self.index_commit(&commit_sha)?;
        self.index.set_tip(&current.header.id, Some(&commit_sha))?;
        Ok(EditOutcome::Written(commit_sha))
    }

    /// Delete `current` by writing a parentless tombstone: `type`,
    /// `id`, `created`, and `modified` = `deleted` = now. Content,
    /// `parent`, and link files are dropped, so prior commits become
    /// unreachable from the ref. Returns the tombstone's SHA.
    pub(crate) fn delete(&self, current: &Object, message: &str) -> Result<String, StoreError> {
        if current.header.is_tombstone() {
            return Err(StoreError::ObjectDeleted(current.header.id.clone()));
        }
        let path = self.path();
        let now = now_ms();
        let stamp_sha = write_blob(path, format!("{now}\n").as_bytes())?;
        let entries = vec![
            blob_entry(&current.marker_sha("type")?, "type"),
            blob_entry(&current.marker_sha("id")?, "id"),
            blob_entry(&current.marker_sha("created")?, "created"),
            blob_entry(&stamp_sha, "modified"),
            blob_entry(&stamp_sha, "deleted"),
        ];
        let tree_sha = mktree(path, &entries)?;
        let commit_sha = commit_tree(path, &tree_sha, message, &[])?;
        run(git_in(
            path,
            [
                "update-ref",
                &object_ref(&current.header.id),
                &commit_sha,
                &current.commit_sha,
            ],
        ))?;
        self.index_commit(&commit_sha)?;
        self.index.set_tip(&current.header.id, Some(&commit_sha))?;
        Ok(commit_sha)
    }

    /// List every ref under `refs/gage/object/` with its id and tip
    /// SHA.
    pub fn list_object_refs(&self) -> Result<Vec<ObjectRef>, StoreError> {
        let out = run(git_in(
            self.path(),
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/gage/object/",
            ],
        ))?;
        let mut refs = Vec::new();
        for line in out.lines() {
            let (name, sha) = line
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
            let Some(id) = name.strip_prefix("refs/gage/object/") else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            refs.push(ObjectRef {
                ref_name: name.to_string(),
                id: id.to_string(),
                tip_sha: sha.to_string(),
            });
        }
        Ok(refs)
    }

    /// Read a commit's markers only.
    pub fn read_header(&self, commit: &str) -> Result<ObjectHeader, StoreError> {
        let entries = read_entries(self.path(), commit)?;
        self.header_from_entries(commit, &entries)
    }

    fn header_from_entries(
        &self,
        commit: &str,
        entries: &BTreeMap<String, TreeEntryRef>,
    ) -> Result<ObjectHeader, StoreError> {
        let type_blob = self
            .read_marker(commit, entries, "type")?
            .ok_or_else(|| StoreError::Parse(format!("missing type blob at {commit}")))?;
        let (object_type, version) = type_blob
            .trim()
            .split_once(' ')
            .map(|(t, v)| (t.to_string(), v.to_string()))
            .ok_or_else(|| StoreError::Parse(format!("type blob at {commit}: {type_blob:?}")))?;
        let id = self
            .read_marker(commit, entries, "id")?
            .ok_or_else(|| StoreError::Parse(format!("missing id blob at {commit}")))?
            .trim()
            .to_string();
        let created_ms = self.read_marker_ms(commit, entries, "created")?;
        let modified_ms = self.read_marker_ms(commit, entries, "modified")?;
        let deleted_ms = self.read_marker_ms(commit, entries, "deleted")?;
        let parent = self
            .read_marker(commit, entries, "parent")?
            .map(|s| s.trim().to_string());
        Ok(ObjectHeader {
            object_type,
            version,
            id,
            created_ms,
            modified_ms,
            deleted_ms,
            parent,
        })
    }

    fn read_marker(
        &self,
        commit: &str,
        entries: &BTreeMap<String, TreeEntryRef>,
        name: &str,
    ) -> Result<Option<String>, StoreError> {
        let Some(entry) = entries.get(name) else {
            return Ok(None);
        };
        if entry.mode != "100644" {
            return Err(StoreError::Parse(format!(
                "marker {name} at {commit} has mode {}",
                entry.mode
            )));
        }
        let bytes = self.read_blob_bytes(&entry.sha)?;
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn read_marker_ms(
        &self,
        commit: &str,
        entries: &BTreeMap<String, TreeEntryRef>,
        name: &str,
    ) -> Result<Option<i64>, StoreError> {
        let Some(text) = self.read_marker(commit, entries, name)? else {
            return Ok(None);
        };
        let ms: i64 = text
            .trim()
            .parse()
            .map_err(|e| StoreError::Parse(format!("{name} at {commit}: {e}")))?;
        Ok(Some(ms))
    }

    /// Walk the commit tree recursively and return every `.link` file
    /// with its listed SHAs. The path is relative to the commit's root.
    pub fn find_link_files(&self, commit: &str) -> Result<Vec<LinkFile>, StoreError> {
        let out = run(git_in(
            self.path(),
            ["ls-tree", "-r", "--name-only", commit],
        ))?;
        let mut files = Vec::new();
        for name in out.lines() {
            if !name.ends_with(".link") {
                continue;
            }
            let bytes = self.read_blob_bytes(&format!("{commit}:{name}"))?;
            files.push(LinkFile {
                path: name.to_string(),
                shas: parse_link(&bytes),
            });
        }
        Ok(files)
    }

    /// Split a commit's parents into `parent`, link parents (attributed
    /// to their link file), and any residuals. `unattributed` and
    /// `missing` are empty in a well-formed commit; the viewer surfaces
    /// them as warnings so structural anomalies are visible.
    pub fn classify_parents(&self, commit: &str) -> Result<ClassifiedParents, StoreError> {
        let meta = self.read_commit(commit)?;
        let header = self.read_header(commit)?;
        let link_files = self.find_link_files(commit)?;

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
            .parent
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
            parent: header.parent,
            links,
            unattributed,
            missing,
        })
    }

    /// Walk the `parent` chain from `commit`, oldest last (index 0 is
    /// `commit` itself). Stops at the first commit whose header has no
    /// `parent` blob.
    pub fn walk_parent_chain(&self, commit: &str) -> Result<Vec<String>, StoreError> {
        let mut chain = Vec::new();
        let mut current = commit.to_string();
        loop {
            chain.push(current.clone());
            let header = self.read_header(&current)?;
            match header.parent {
                Some(parent) if !parent.is_empty() => current = parent,
                _ => return Ok(chain),
            }
        }
    }
}

/// Fail unless the object's `type` is `expected`.
pub(crate) fn require_type(object: &Object, expected: &str) -> Result<(), StoreError> {
    if object.header.object_type == expected {
        return Ok(());
    }
    Err(StoreError::WrongType {
        id: object.header.id.clone(),
        expected: expected.to_string(),
        actual: object.header.object_type.clone(),
    })
}

impl Object {
    /// An in-memory object with only a type and attrs, for tests that
    /// exercise attribute extraction without a repository.
    #[cfg(test)]
    pub(crate) fn for_test(object_type: &str, attrs: JsonValue) -> Object {
        Object {
            commit_sha: String::new(),
            header: ObjectHeader {
                object_type: object_type.to_string(),
                version: "1".to_string(),
                id: String::new(),
                created_ms: None,
                modified_ms: None,
                deleted_ms: None,
                parent: None,
            },
            tree: ObjectTree {
                attrs: Some(attrs),
                ..ObjectTree::default()
            },
            entries: BTreeMap::new(),
        }
    }

    fn marker_sha(&self, name: &str) -> Result<String, StoreError> {
        self.entries
            .get(name)
            .map(|e| e.sha.clone())
            .ok_or_else(|| StoreError::Parse(format!("missing {name} blob at {}", self.commit_sha)))
    }
}

/// Content entries written for an [`ObjectTree`], with the SHAs that
/// identify the content and the SHAs that become link parents.
struct WrittenContent {
    entries: Vec<String>,
    /// Entry name to object SHA, for change detection.
    shas: BTreeMap<String, String>,
    link_parents: Vec<String>,
}

fn write_content(path: &Path, tree: &ObjectTree) -> Result<WrittenContent, StoreError> {
    let mut entries = Vec::new();
    let mut shas = BTreeMap::new();
    let mut link_parents = Vec::new();
    if let Some(attrs) = &tree.attrs {
        let mut json = serde_json::to_string(attrs)
            .map_err(|e| StoreError::Parse(format!("attrs.json encode: {e}")))?;
        json.push('\n');
        let sha = write_blob(path, json.as_bytes())?;
        entries.push(blob_entry(&sha, "attrs.json"));
        shas.insert("attrs.json".to_string(), sha);
    }
    for (name, bytes) in &tree.blobs {
        let sha = write_blob(path, bytes)?;
        entries.push(blob_entry(&sha, name));
        shas.insert(name.clone(), sha);
    }
    for (name, link_shas) in &tree.links {
        let content: String = link_shas.iter().map(|s| format!("{s}\n")).collect();
        let sha = write_blob(path, content.as_bytes())?;
        entries.push(blob_entry(&sha, name));
        shas.insert(name.clone(), sha);
        link_parents.extend(link_shas.iter().cloned());
    }
    for (name, sha) in &tree.subtrees {
        entries.push(format!("040000 tree {sha}\t{name}"));
        shas.insert(name.clone(), sha.clone());
    }
    Ok(WrittenContent {
        entries,
        shas,
        link_parents,
    })
}

fn content_shas(entries: &BTreeMap<String, TreeEntryRef>) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter(|(name, _)| !MARKERS.contains(&name.as_str()))
        .map(|(name, e)| (name.clone(), e.sha.clone()))
        .collect()
}

fn blob_entry(sha: &str, name: &str) -> String {
    format!("100644 blob {sha}\t{name}")
}

fn parse_link(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn read_entries(path: &Path, commit: &str) -> Result<BTreeMap<String, TreeEntryRef>, StoreError> {
    let listing = run(git_in(path, ["ls-tree", commit]))?;
    let mut entries = BTreeMap::new();
    for line in listing.lines() {
        let (meta, name) = line
            .split_once('\t')
            .ok_or_else(|| StoreError::Parse(format!("ls-tree line: {line}")))?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind, sha]: [&str; 3] = fields.try_into().map_err(|got: Vec<&str>| {
            StoreError::Parse(format!(
                "ls-tree meta {meta:?}: expected 3 fields, got {}",
                got.len()
            ))
        })?;
        entries.insert(
            name.to_string(),
            TreeEntryRef {
                mode: mode.to_string(),
                kind: kind.to_string(),
                sha: sha.to_string(),
            },
        );
    }
    Ok(entries)
}

/// The parents of one commit, split into the previous-version parent
/// (from the `parent` blob) and the link parents (each attributed to
/// the link file that named it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedParents {
    /// SHA named by the `parent` blob, when present.
    pub parent: Option<String>,
    /// Link parents attributed to the link file that named them.
    pub links: Vec<LinkParent>,
    /// Commit parents that appear in the commit but do not match
    /// `parent` or any listed link file. Empty when everything checks
    /// out.
    pub unattributed: Vec<String>,
    /// SHAs named by `parent` or a link file that are not commit
    /// parents. Empty when everything checks out.
    pub missing: Vec<String>,
}

/// One SHA named by one link file, and the path of that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkParent {
    pub link_file: String,
    pub sha: String,
}

/// The full contents of one link file discovered under a commit tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkFile {
    /// Path relative to the commit tree root.
    pub path: String,
    /// SHAs listed in the file, in file order.
    pub shas: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::{NoteInput, NoteStore};
    use crate::{DatasetStore, init};
    use serde_json::json;

    fn open_store(dir: &Path) -> Store {
        let path = dir.join("store.git");
        init(&path).unwrap();
        Store::open(&path).unwrap()
    }

    fn tree_names(store: &Store, commit: &str) -> Vec<String> {
        run(git_in(store.path(), ["ls-tree", "--name-only", commit]))
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    fn cat(store: &Store, spec: &str) -> String {
        run(git_in(store.path(), ["cat-file", "-p", spec])).unwrap()
    }

    fn commit_parents(store: &Store, commit: &str) -> Vec<String> {
        store.read_commit(commit).unwrap().parents
    }

    fn note(store: &Store, name: &str, targets: &[String]) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: "v",
                author: "user:test",
                targets,
            })
            .unwrap()
    }

    #[test]
    fn create_writes_markers_content_and_link_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let linked = note(&store, "linked", &[]);
        let linked_sha = store.resolve_id(&linked).unwrap().1;

        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "b": 1, "a": "x" }));
        tree.blobs.insert("body.txt".into(), b"hello\n".to_vec());
        tree.links
            .insert("things.link".into(), vec![linked_sha.clone()]);
        let sha = store
            .create("gage::test", "7", "abc", &tree, "test: create")
            .unwrap();

        assert_eq!(
            tree_names(&store, &sha),
            vec![
                "attrs.json",
                "body.txt",
                "created",
                "id",
                "modified",
                "things.link",
                "type"
            ]
        );
        assert_eq!(cat(&store, &format!("{sha}:type")), "gage::test 7\n");
        assert_eq!(cat(&store, &format!("{sha}:id")), "abc\n");
        assert_eq!(
            cat(&store, &format!("{sha}:attrs.json")),
            "{\"a\":\"x\",\"b\":1}\n"
        );
        assert_eq!(cat(&store, &format!("{sha}:body.txt")), "hello\n");
        assert_eq!(
            cat(&store, &format!("{sha}:created")),
            cat(&store, &format!("{sha}:modified"))
        );
        assert_eq!(commit_parents(&store, &sha), vec![linked_sha]);
        assert_eq!(store.resolve_id("abc").unwrap().1, sha);
    }

    #[test]
    fn read_object_round_trips_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let linked = note(&store, "linked", &[]);
        let linked_sha = store.resolve_id(&linked).unwrap().1;

        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "n": 2 }));
        tree.blobs.insert("body.txt".into(), b"hello\n".to_vec());
        tree.links.insert("things.link".into(), vec![linked_sha]);
        let sha = store
            .create("gage::test", "1", "abc", &tree, "test")
            .unwrap();

        let object = store.read_object(&sha).unwrap();
        assert_eq!(object.commit_sha, sha);
        assert_eq!(object.header.object_type, "gage::test");
        assert_eq!(object.header.version, "1");
        assert_eq!(object.header.id, "abc");
        assert!(object.header.parent.is_none());
        assert!(!object.header.is_tombstone());
        assert_eq!(object.tree, tree);
    }

    #[test]
    fn edit_writes_parent_first_then_links_and_bumps_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let linked = note(&store, "linked", &[]);
        let linked_sha = store.resolve_id(&linked).unwrap().1;

        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "n": 1 }));
        tree.links
            .insert("things.link".into(), vec![linked_sha.clone()]);
        let first = store
            .create("gage::test", "1", "abc", &tree, "test")
            .unwrap();
        let current = store.read_object(&first).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(2));
        tree.attrs = Some(json!({ "n": 2 }));
        let outcome = store.edit(&current, &tree, "test: edit").unwrap();
        let EditOutcome::Written(second) = outcome else {
            panic!("expected a written edit, got {outcome:?}");
        };

        assert_eq!(
            commit_parents(&store, &second),
            vec![first.clone(), linked_sha]
        );
        assert_eq!(
            cat(&store, &format!("{second}:parent")),
            format!("{first}\n")
        );
        assert_eq!(
            cat(&store, &format!("{second}:created")),
            cat(&store, &format!("{first}:created"))
        );
        assert_ne!(
            cat(&store, &format!("{second}:modified")),
            cat(&store, &format!("{first}:modified"))
        );
        assert_eq!(cat(&store, &format!("{second}:attrs.json")), "{\"n\":2}\n");
        assert_eq!(store.resolve_id("abc").unwrap().1, second);

        let edited = store.read_object(&second).unwrap();
        assert_eq!(edited.header.parent.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn edit_with_identical_content_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "n": 1 }));
        tree.blobs.insert("body.txt".into(), b"x".to_vec());
        let first = store
            .create("gage::test", "1", "abc", &tree, "test")
            .unwrap();
        let current = store.read_object(&first).unwrap();

        assert_eq!(
            store.edit(&current, &tree, "test: edit").unwrap(),
            EditOutcome::Unchanged
        );
        assert_eq!(store.resolve_id("abc").unwrap().1, first);
    }

    #[test]
    fn delete_writes_parentless_tombstone_with_markers_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "n": 1 }));
        tree.blobs.insert("body.txt".into(), b"x".to_vec());
        let first = store
            .create("gage::test", "1", "abc", &tree, "test")
            .unwrap();
        let current = store.read_object(&first).unwrap();

        let tomb = store.delete(&current, "test: delete").unwrap();
        assert!(commit_parents(&store, &tomb).is_empty());
        assert_eq!(
            tree_names(&store, &tomb),
            vec!["created", "deleted", "id", "modified", "type"]
        );
        assert_eq!(
            cat(&store, &format!("{tomb}:created")),
            cat(&store, &format!("{first}:created"))
        );
        assert_eq!(
            cat(&store, &format!("{tomb}:deleted")),
            cat(&store, &format!("{tomb}:modified"))
        );
        let object = store.read_object(&tomb).unwrap();
        assert!(object.header.is_tombstone());
        assert_eq!(object.tree, ObjectTree::default());

        assert!(matches!(
            store.edit(&object, &tree, "x").unwrap_err(),
            StoreError::ObjectDeleted(id) if id == "abc"
        ));
        assert!(matches!(
            store.delete(&object, "x").unwrap_err(),
            StoreError::ObjectDeleted(id) if id == "abc"
        ));
    }

    #[test]
    fn resolve_id_reports_missing_and_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let tree = ObjectTree::default();
        store.create("gage::test", "1", "abc1", &tree, "t").unwrap();
        store.create("gage::test", "1", "abc2", &tree, "t").unwrap();

        assert!(matches!(
            store.resolve_id("zzz").unwrap_err(),
            StoreError::ObjectNotFound(p) if p == "zzz"
        ));
        assert!(matches!(
            store.resolve_id("abc").unwrap_err(),
            StoreError::AmbiguousId(p, 2) if p == "abc"
        ));
        assert_eq!(store.resolve_id("abc1").unwrap().0, "abc1");
    }

    #[test]
    fn resolve_typed_checks_type_and_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let sha = store
            .create("gage::test", "1", "abc", &ObjectTree::default(), "t")
            .unwrap();
        assert_eq!(
            store.resolve_typed("abc", "gage::test").unwrap().commit_sha,
            sha
        );
        assert!(matches!(
            store.resolve_typed("abc", "gage::note").unwrap_err(),
            StoreError::WrongType { id, expected, actual }
                if id == "abc" && expected == "gage::note" && actual == "gage::test"
        ));
        let current = store.read_object(&sha).unwrap();
        store.delete(&current, "t").unwrap();
        assert!(matches!(
            store.resolve_typed("abc", "gage::test").unwrap_err(),
            StoreError::ObjectDeleted(id) if id == "abc"
        ));
    }

    #[test]
    fn list_object_refs_returns_every_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let note_id = note(&store, "n", &[]);
        let dataset_id = DatasetStore::from(&store).create().unwrap();

        let refs = store.list_object_refs().unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.ref_name.as_str()).collect();
        assert!(names.contains(&object_ref(&note_id).as_str()));
        assert!(names.contains(&object_ref(&dataset_id).as_str()));
    }

    #[test]
    fn classify_parents_splits_parent_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let notes = NoteStore::from(&store);

        let root = note(&store, "root", &[]);
        let root_commit = store.resolve_id(&root).unwrap().1;
        let child = note(&store, "reply", &[format!("note:{root}")]);
        let first_child = store.resolve_id(&child).unwrap().1;
        notes.edit(&child, "second").unwrap();

        let classified = store.classify_parents(&object_ref(&child)).unwrap();
        assert_eq!(classified.parent.as_deref(), Some(first_child.as_str()));
        assert_eq!(classified.links.len(), 1);
        assert_eq!(classified.links[0].link_file, "target.link");
        assert_eq!(classified.links[0].sha, root_commit);
        assert!(classified.unattributed.is_empty());
        assert!(classified.missing.is_empty());
    }

    #[test]
    fn walk_parent_chain_returns_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let notes = NoteStore::from(&store);

        let id = note(&store, "n", &[]);
        notes.edit(&id, "v2").unwrap();
        notes.edit(&id, "v3").unwrap();

        let tip = store.resolve_id(&id).unwrap().1;
        let chain = store.walk_parent_chain(&tip).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], tip);
    }

    #[test]
    fn find_link_files_reads_target_link() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());

        let root = note(&store, "root", &[]);
        let root_commit = store.resolve_id(&root).unwrap().1;
        let child = note(&store, "reply", &[format!("note:{root}")]);
        let files = store.find_link_files(&object_ref(&child)).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "target.link");
        assert_eq!(files[0].shas, vec![root_commit]);
    }
}
