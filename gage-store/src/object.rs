//! The Gage object model: one tree grammar and one create, edit, and
//! delete path for every object type.
//!
//! An object is a commit under `refs/gage/object/<id>`. Its tree
//! carries the marker files `type` (`gage::<name> <version>`), `id`,
//! `created`, `modified`, and, on a tombstone, `deleted`; an edit adds
//! `parent` holding the previous version's SHA. Type-specific content
//! is `attrs.json`, blob-valued attribute files (`value.txt`), link
//! files (`*.link`, one commit SHA per line), and opaque subtrees
//! whose name ends in `.d` (`files.d/`). Gage schema walkers stop at
//! the root of a `.d` subtree; its interior is producer-owned. See
//! README: Appendix: Object tree layout.
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

use crate::git::{EntryKind, git_in, run};
use crate::writer::{TreeInput, commit_tree, mktree, write_blob};
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
    ///
    /// This is the one read still on a `git` launch, a `for-each-ref`
    /// glob whose cost grows with the loose ref count until `gc`. It
    /// is why an edit or delete costs more than a create; see footnote
    /// 5 of `gage-bench/results/store/README.md`. The index holds the
    /// ref table and is the known replacement.
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
        let commit_sha = self
            .object_info(&format!("{commit}^{{commit}}"))?
            .ok_or_else(|| StoreError::MissingObject(commit.to_string()))?
            .sha;
        let entries = self.read_entries(&commit_sha)?;
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
                    tree.links
                        .insert(name.clone(), parse_link(&bytes, name, &commit_sha)?);
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

        let content = write_content(path, tree)?;
        let mut entries = content.entries;
        entries.insert("type".to_string(), blob_entry(&type_sha));
        entries.insert("id".to_string(), blob_entry(&id_sha));
        entries.insert("created".to_string(), blob_entry(&stamp_sha));
        entries.insert("modified".to_string(), blob_entry(&stamp_sha));
        let tree_sha = mktree(path, &tree_lines(&entries))?;

        let links = self.link_files_for_write(tree, &tree_sha)?;
        let parents = link_parents(None, &links);
        let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
        let commit_sha = commit_tree(path, &tree_sha, message, &parents)?;
        let object = Object {
            commit_sha: commit_sha.clone(),
            header: ObjectHeader {
                object_type: object_type.to_string(),
                version: version.to_string(),
                id: id.to_string(),
                created_ms: Some(now),
                modified_ms: Some(now),
                deleted_ms: None,
                parent: None,
            },
            tree: tree.clone(),
            entries,
        };
        self.record_write(&object, &links, "")?;
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
        let mut entries = content.entries;
        entries.insert("type".to_string(), current.marker("type")?);
        entries.insert("id".to_string(), current.marker("id")?);
        entries.insert("created".to_string(), current.marker("created")?);
        entries.insert("modified".to_string(), blob_entry(&modified_sha));
        entries.insert("parent".to_string(), blob_entry(&parent_sha));
        let tree_sha = mktree(path, &tree_lines(&entries))?;

        let links = self.link_files_for_write(tree, &tree_sha)?;
        let parents = link_parents(Some(&current.commit_sha), &links);
        let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
        let commit_sha = commit_tree(path, &tree_sha, message, &parents)?;
        let object = Object {
            commit_sha: commit_sha.clone(),
            header: ObjectHeader {
                modified_ms: Some(now),
                deleted_ms: None,
                parent: Some(current.commit_sha.clone()),
                ..current.header.clone()
            },
            tree: tree.clone(),
            entries,
        };
        self.record_write(&object, &links, &current.commit_sha)?;
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
        let mut entries = BTreeMap::new();
        entries.insert("type".to_string(), current.marker("type")?);
        entries.insert("id".to_string(), current.marker("id")?);
        entries.insert("created".to_string(), current.marker("created")?);
        entries.insert("modified".to_string(), blob_entry(&stamp_sha));
        entries.insert("deleted".to_string(), blob_entry(&stamp_sha));
        let tree_sha = mktree(path, &tree_lines(&entries))?;
        let commit_sha = commit_tree(path, &tree_sha, message, &[])?;
        let object = Object {
            commit_sha: commit_sha.clone(),
            header: ObjectHeader {
                modified_ms: Some(now),
                deleted_ms: Some(now),
                parent: None,
                ..current.header.clone()
            },
            tree: ObjectTree::default(),
            entries,
        };
        self.record_write(&object, &[], &current.commit_sha)?;
        self.deleted.set(true);
        Ok(commit_sha)
    }

    /// Supersede the tombstone `current` with a parentless live commit
    /// carrying `tree` as its content. `type`, `id`, and `created` are
    /// the tombstone's entries, so the object keeps its identity;
    /// `modified` is now; `deleted` and `parent` are absent. Prior
    /// content is not restored. Applies to objects whose id is derived
    /// from external inputs, where re-adding the same input after a
    /// delete names the same object. Returns the new commit's SHA.
    pub(crate) fn resurrect(
        &self,
        current: &Object,
        tree: &ObjectTree,
        message: &str,
    ) -> Result<String, StoreError> {
        if !current.header.is_tombstone() {
            return Err(StoreError::ObjectLive(current.header.id.clone()));
        }
        let path = self.path();
        let now = now_ms();
        let modified_sha = write_blob(path, format!("{now}\n").as_bytes())?;
        let content = write_content(path, tree)?;
        let mut entries = content.entries;
        entries.insert("type".to_string(), current.marker("type")?);
        entries.insert("id".to_string(), current.marker("id")?);
        entries.insert("created".to_string(), current.marker("created")?);
        entries.insert("modified".to_string(), blob_entry(&modified_sha));
        let tree_sha = mktree(path, &tree_lines(&entries))?;

        let links = self.link_files_for_write(tree, &tree_sha)?;
        let parents = link_parents(None, &links);
        let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
        let commit_sha = commit_tree(path, &tree_sha, message, &parents)?;
        let object = Object {
            commit_sha: commit_sha.clone(),
            header: ObjectHeader {
                modified_ms: Some(now),
                deleted_ms: None,
                parent: None,
                ..current.header.clone()
            },
            tree: tree.clone(),
            entries,
        };
        self.record_write(&object, &links, &current.commit_sha)?;
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
        let entries = self.read_entries(commit)?;
        self.header_from_entries(commit, &entries)
    }

    /// Top-level entries of `commit`'s tree by name.
    fn read_entries(&self, commit: &str) -> Result<BTreeMap<String, TreeEntryRef>, StoreError> {
        Ok(self
            .read_tree(commit)?
            .into_iter()
            .map(|e| {
                (
                    e.name,
                    TreeEntryRef {
                        mode: e.mode,
                        kind: e.kind.as_str().to_string(),
                        sha: e.sha,
                    },
                )
            })
            .collect())
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
        let parent = match self.read_marker(commit, entries, "parent")? {
            Some(text) => Some(parse_sha(text.trim(), "parent blob", commit)?),
            None => None,
        };
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

    /// Every link file a write carries, for its commit parents and its
    /// index rows. The root links are in `tree`; a subtree that is not
    /// `.d` is walked at `tree_sha`, the same walk a rebuild uses, so
    /// the write and the rebuild agree.
    fn link_files_for_write(
        &self,
        tree: &ObjectTree,
        tree_sha: &str,
    ) -> Result<Vec<LinkFile>, StoreError> {
        if tree.subtrees.keys().all(|name| name.ends_with(".d")) {
            Ok(tree
                .links
                .iter()
                .map(|(path, shas)| LinkFile {
                    path: path.clone(),
                    shas: shas.clone(),
                })
                .collect())
        } else {
            self.find_link_files(tree_sha)
        }
    }

    /// Walk the commit tree and return every `.link` file with its
    /// listed SHAs. The path is relative to the commit's root.
    ///
    /// Subtrees whose name ends in `.d` are opaque: their contents are
    /// producer-owned and Gage schema walkers stop at the root of such
    /// a subtree. A `.link` blob inside a `.d` subtree is producer
    /// content, not a Gage link.
    pub fn find_link_files(&self, commit: &str) -> Result<Vec<LinkFile>, StoreError> {
        let mut link_blobs: Vec<(String, String)> = Vec::new();
        self.walk_link_scannable(commit, "", &mut link_blobs)?;
        let mut files = Vec::with_capacity(link_blobs.len());
        for (path, sha) in link_blobs {
            let bytes = self.read_blob_bytes(&sha)?;
            let shas = parse_link(&bytes, &path, commit)?;
            files.push(LinkFile { path, shas });
        }
        Ok(files)
    }

    fn walk_link_scannable(
        &self,
        tree_ish: &str,
        prefix: &str,
        out: &mut Vec<(String, String)>,
    ) -> Result<(), StoreError> {
        for entry in self.read_tree(tree_ish)? {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match entry.kind {
                EntryKind::Blob if entry.name.ends_with(".link") => {
                    out.push((path, entry.sha));
                }
                EntryKind::Tree if !entry.name.ends_with(".d") => {
                    self.walk_link_scannable(&entry.sha, &path, out)?;
                }
                _ => {}
            }
        }
        Ok(())
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
    /// `parent` blob. The walk terminates because every `parent` is a
    /// SHA: a cycle would need a commit whose SHA covers a blob naming
    /// a commit whose SHA covers a blob naming it back.
    pub fn walk_parent_chain(&self, commit: &str) -> Result<Vec<String>, StoreError> {
        let mut chain = Vec::new();
        let mut current = commit.to_string();
        loop {
            chain.push(current.clone());
            match self.read_header(&current)?.parent {
                Some(parent) => current = parent,
                None => return Ok(chain),
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

    /// The tree entry of marker `name`, for reuse in a child commit.
    fn marker(&self, name: &str) -> Result<TreeEntryRef, StoreError> {
        self.entries
            .get(name)
            .cloned()
            .ok_or_else(|| StoreError::Parse(format!("missing {name} blob at {}", self.commit_sha)))
    }
}

/// Content entries written for an [`ObjectTree`], with the SHAs that
/// identify the content.
struct WrittenContent {
    /// Content entries by name.
    entries: BTreeMap<String, TreeEntryRef>,
    /// Entry name to object SHA, for change detection.
    shas: BTreeMap<String, String>,
}

fn write_content(path: &Path, tree: &ObjectTree) -> Result<WrittenContent, StoreError> {
    let mut entries = BTreeMap::new();
    let mut shas = BTreeMap::new();
    if let Some(attrs) = &tree.attrs {
        let mut json = serde_json::to_string(attrs)
            .map_err(|e| StoreError::Parse(format!("attrs.json encode: {e}")))?;
        json.push('\n');
        let sha = write_blob(path, json.as_bytes())?;
        entries.insert("attrs.json".to_string(), blob_entry(&sha));
        shas.insert("attrs.json".to_string(), sha);
    }
    for (name, bytes) in &tree.blobs {
        let sha = write_blob(path, bytes)?;
        entries.insert(name.clone(), blob_entry(&sha));
        shas.insert(name.clone(), sha);
    }
    for (name, link_shas) in &tree.links {
        let content: String = link_shas.iter().map(|s| format!("{s}\n")).collect();
        let sha = write_blob(path, content.as_bytes())?;
        entries.insert(name.clone(), blob_entry(&sha));
        shas.insert(name.clone(), sha);
    }
    for (name, sha) in &tree.subtrees {
        entries.insert(
            name.clone(),
            TreeEntryRef {
                mode: "040000".to_string(),
                kind: "tree".to_string(),
                sha: sha.clone(),
            },
        );
        shas.insert(name.clone(), sha.clone());
    }
    Ok(WrittenContent { entries, shas })
}

/// The commit parents for a write: the previous version first, then
/// every SHA in every link file in file order. Every link SHA becomes a
/// parent, which is what makes a link a link: the target stays
/// reachable, and a fetch of this object carries it. Repeats are
/// written once.
fn link_parents(previous: Option<&str>, links: &[LinkFile]) -> Vec<String> {
    let mut parents: Vec<String> = Vec::new();
    for sha in previous
        .into_iter()
        .chain(links.iter().flat_map(|l| l.shas.iter().map(String::as_str)))
    {
        if !parents.iter().any(|p| p == sha) {
            parents.push(sha.to_string());
        }
    }
    parents
}

/// `mktree` input for `entries`.
fn tree_lines(entries: &BTreeMap<String, TreeEntryRef>) -> Vec<TreeInput<'_>> {
    entries
        .iter()
        .map(|(name, e)| TreeInput {
            mode: &e.mode,
            sha: &e.sha,
            name,
        })
        .collect()
}

fn content_shas(entries: &BTreeMap<String, TreeEntryRef>) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter(|(name, _)| !MARKERS.contains(&name.as_str()))
        .map(|(name, e)| (name.clone(), e.sha.clone()))
        .collect()
}

fn blob_entry(sha: &str) -> TreeEntryRef {
    TreeEntryRef {
        mode: "100644".to_string(),
        kind: "blob".to_string(),
        sha: sha.to_string(),
    }
}

/// The commit SHAs listed in a link file, one per line. Every line
/// must be a SHA: a ref name would resolve, and a ref that points back
/// at the object makes every walk over parents and links a loop.
fn parse_link(bytes: &[u8], path: &str, commit: &str) -> Result<Vec<String>, StoreError> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|line| parse_sha(line, &format!("link file {path}"), commit))
        .collect()
}

/// `value` when it is a 40-character lowercase hex SHA, as the store
/// writes them. `what` names the source for the error.
fn parse_sha(value: &str, what: &str, commit: &str) -> Result<String, StoreError> {
    let is_sha = value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if is_sha {
        Ok(value.to_string())
    } else {
        Err(StoreError::Parse(format!(
            "{what} at {commit}: {value:?} is not a commit SHA"
        )))
    }
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
    use crate::DatasetStore;
    use crate::note::{NoteInput, NoteStore, NoteValue};
    use crate::test_support::open_store;
    use serde_json::json;

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

    /// Rewrites the object at `id` so its tree carries `name` as a blob
    /// with `content`, bypassing the writer's checks.
    fn plant_blob(store: &Store, id: &str, name: &str, content: &str) -> String {
        let (_, sha) = store.resolve_id(id).unwrap();
        let mut entries = store.read_entries(&sha).unwrap();
        let blob = write_blob(store.path(), content.as_bytes()).unwrap();
        entries.insert(name.to_string(), blob_entry(&blob));
        let tree = mktree(store.path(), &tree_lines(&entries)).unwrap();
        let commit = commit_tree(store.path(), &tree, "planted", &[]).unwrap();
        run(git_in(
            store.path(),
            ["update-ref", &object_ref(id), &commit],
        ))
        .unwrap();
        commit
    }

    #[test]
    fn parent_naming_a_ref_is_a_parse_error_not_a_loop() {
        // Both the chain walk and the reconcile at open follow
        // `parent`. A regression hangs, so the whole test runs on a
        // thread with a bound.
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let tmp = tempfile::tempdir().unwrap();
            let (store, _fsck) = open_store(tmp.path());
            let id = note(&store, "n", None);
            let commit = plant_blob(&store, &id, "parent", &format!("{}\n", object_ref(&id)));
            let walk = store.walk_parent_chain(&commit).map_err(|e| e.to_string());
            let path = store.path().to_path_buf();
            drop(store);
            let open = Store::open(&path).map(|_| ()).map_err(|e| e.to_string());
            send.send((walk, open)).unwrap();
        });
        let (walk, open) = recv
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("walk and open should return, not loop");
        match walk {
            Err(e) => assert!(e.contains("parent blob"), "{e}"),
            Ok(chain) => panic!("expected a parse error, got {chain:?}"),
        }
        match open {
            Err(e) => assert!(e.contains("parent blob"), "{e}"),
            Ok(()) => panic!("expected open to fail with a parse error"),
        }
    }

    #[test]
    fn link_line_naming_a_ref_is_a_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "n", None);
        let commit = plant_blob(&store, &id, "things.link", "refs/gage/object/x\n");

        match store.read_object(&commit) {
            Err(StoreError::Parse(what)) => {
                assert!(what.contains("link file things.link"), "{what}");
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    /// A `tasks/x/` subtree holding `agent_sessions.link` listing
    /// `shas`, beside an opaque `logs.d` with a decoy `.link`.
    fn nested_link_tree(store: &Store, shas: &[&str]) -> String {
        let listing: String = shas.iter().map(|s| format!("{s}\n")).collect();
        let link_blob = write_blob(store.path(), listing.as_bytes()).unwrap();
        let decoy_blob = write_blob(store.path(), b"not a sha\n").unwrap();
        let logs = mktree(
            store.path(),
            &[TreeInput {
                mode: "100644",
                sha: &decoy_blob,
                name: "decoy.link",
            }],
        )
        .unwrap();
        let task = mktree(
            store.path(),
            &[
                TreeInput {
                    mode: "100644",
                    sha: &link_blob,
                    name: "agent_sessions.link",
                },
                TreeInput {
                    mode: "040000",
                    sha: &logs,
                    name: "logs.d",
                },
            ],
        )
        .unwrap();
        mktree(
            store.path(),
            &[TreeInput {
                mode: "040000",
                sha: &task,
                name: "x",
            }],
        )
        .unwrap()
    }

    #[test]
    fn nested_links_become_commit_parents_on_create_and_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let a = store.resolve_id(&note(&store, "a", None)).unwrap().1;
        let b = store.resolve_id(&note(&store, "b", None)).unwrap().1;
        // The nested link lists b and a; the root link lists a. Every
        // SHA becomes a parent, and a is written once. Git sorts
        // `tasks/` before `things.link`, so the nested SHAs come first.
        let tasks = nested_link_tree(&store, &[&b, &a]);
        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "n": 1 }));
        tree.links.insert("things.link".into(), vec![a.clone()]);
        tree.subtrees.insert("tasks".to_string(), tasks);

        let first = store
            .create("gage::test", "1", "scan", &tree, "create")
            .unwrap();
        assert_eq!(commit_parents(&store, &first), vec![b.clone(), a.clone()]);
        let classified = store.classify_parents(&first).unwrap();
        assert!(classified.missing.is_empty(), "{classified:?}");
        assert!(classified.unattributed.is_empty(), "{classified:?}");
        assert!(
            classified
                .links
                .iter()
                .any(|l| l.link_file == "tasks/x/agent_sessions.link" && l.sha == b),
            "{classified:?}"
        );

        let current = store.read_object(&first).unwrap();
        let mut edited = tree.clone();
        edited.attrs = Some(json!({ "n": 2 }));
        let EditOutcome::Written(second) = store.edit(&current, &edited, "edit").unwrap() else {
            panic!("edit should write");
        };
        assert_eq!(
            commit_parents(&store, &second),
            vec![first.clone(), b.clone(), a.clone()]
        );
        let classified = store.classify_parents(&second).unwrap();
        assert!(classified.missing.is_empty(), "{classified:?}");
        assert!(classified.unattributed.is_empty(), "{classified:?}");
    }

    /// The expected-old-value argument to `update-ref` is the only
    /// guard against a lost update and a duplicate create.
    #[test]
    fn stale_current_and_duplicate_create_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "a", None);
        let first = store.resolve_id(&id).unwrap().1;
        let stale = store.read_object(&first).unwrap();

        NoteStore::from(&store)
            .edit(&id, &NoteValue::Text("v2".into()))
            .unwrap();
        let second = store.resolve_id(&id).unwrap().1;
        assert_ne!(first, second);

        let mut tree = stale.tree.clone();
        tree.blobs.insert("value.txt".into(), b"v3\n".to_vec());
        assert!(matches!(
            store.edit(&stale, &tree, "stale edit"),
            Err(StoreError::Git { .. })
        ));
        assert!(matches!(
            store.delete(&stale, "stale delete"),
            Err(StoreError::Git { .. })
        ));
        assert!(matches!(
            store.create("gage::note", "1", &id, &tree, "duplicate"),
            Err(StoreError::Git { .. })
        ));

        assert_eq!(store.resolve_id(&id).unwrap().1, second);
        assert_eq!(
            NoteStore::from(&store).get(&id).unwrap().value,
            NoteValue::Text("v2".into())
        );
        let reopened = Store::open(store.path()).unwrap();
        assert_eq!(
            NoteStore::from(&reopened).get(&id).unwrap().value,
            NoteValue::Text("v2".into())
        );
    }

    #[test]
    fn find_link_files_skips_opaque_d_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let target = note(&store, "target", None);
        let target_sha = store.resolve_id(&target).unwrap().1;
        let holder = note(&store, "holder", Some(&format!("note:{target}")));

        // The producer put a `.link`-suffixed blob inside an opaque
        // `.d` subtree. Its content is not a SHA, which would fail
        // `parse_link` if it were read. The walker must not descend.
        let (_, holder_sha) = store.resolve_id(&holder).unwrap();
        let mut entries = store.read_entries(&holder_sha).unwrap();
        let inner_blob = write_blob(store.path(), b"not a sha\n").unwrap();
        let inner_tree = mktree(
            store.path(),
            &[TreeInput {
                mode: "100644",
                sha: &inner_blob,
                name: "leaf.link",
            }],
        )
        .unwrap();
        entries.insert(
            "producer.d".to_string(),
            TreeEntryRef {
                mode: "040000".to_string(),
                kind: "tree".to_string(),
                sha: inner_tree,
            },
        );
        let tree = mktree(store.path(), &tree_lines(&entries)).unwrap();
        let commit = commit_tree(store.path(), &tree, "planted", &[]).unwrap();
        run(git_in(
            store.path(),
            ["update-ref", &object_ref(&holder), &commit],
        ))
        .unwrap();

        let files = store.find_link_files(&commit).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].path, "target.link");
        assert_eq!(files[0].shas, vec![target_sha]);
    }

    fn note(store: &Store, name: &str, target: Option<&str>) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target,
            })
            .unwrap()
    }

    #[test]
    fn create_writes_markers_content_and_link_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let linked = note(&store, "linked", None);
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
        let (store, _fsck) = open_store(tmp.path());
        let linked = note(&store, "linked", None);
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
        let (store, _fsck) = open_store(tmp.path());
        let linked = note(&store, "linked", None);
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
        let (store, _fsck) = open_store(tmp.path());
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
        let (store, _fsck) = open_store(tmp.path());
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
    fn resurrect_writes_parentless_live_commit_with_tombstone_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut tree = ObjectTree::default();
        tree.attrs = Some(json!({ "n": 1 }));
        tree.blobs.insert("body.txt".into(), b"x".to_vec());
        let first = store
            .create("gage::test", "1", "abc", &tree, "test")
            .unwrap();
        let live = store.read_object(&first).unwrap();
        assert!(matches!(
            store.resurrect(&live, &tree, "x").unwrap_err(),
            StoreError::ObjectLive(id) if id == "abc"
        ));

        let tomb = store.delete(&live, "test: delete").unwrap();
        let tombstone = store.read_object(&tomb).unwrap();
        let mut new_tree = ObjectTree::default();
        new_tree.attrs = Some(json!({ "n": 2 }));
        new_tree.blobs.insert("body.txt".into(), b"y".to_vec());

        let revived = store
            .resurrect(&tombstone, &new_tree, "test: resurrect")
            .unwrap();
        assert!(commit_parents(&store, &revived).is_empty());
        assert_eq!(
            tree_names(&store, &revived),
            vec![
                "attrs.json",
                "body.txt",
                "created",
                "id",
                "modified",
                "type"
            ]
        );
        for marker in ["type", "id", "created"] {
            assert_eq!(
                cat(&store, &format!("{revived}:{marker}")),
                cat(&store, &format!("{tomb}:{marker}")),
                "{marker}"
            );
        }
        assert_eq!(cat(&store, &format!("{revived}:body.txt")), "y");
        assert_eq!(
            store.rev_parse(&object_ref("abc")).unwrap().unwrap(),
            revived
        );

        let object = store.read_object(&revived).unwrap();
        assert!(!object.header.is_tombstone());
        assert_eq!(object.header.created_ms, tombstone.header.created_ms);
        assert_eq!(object.header.parent, None);
        assert_eq!(object.tree, new_tree);

        // The revived object edits as any live object, chaining from
        // the resurrection commit
        let edited = match store.edit(&object, &tree, "test: edit").unwrap() {
            EditOutcome::Written(sha) => sha,
            EditOutcome::Unchanged => panic!("content differs"),
        };
        assert_eq!(
            cat(&store, &format!("{edited}:parent")),
            format!("{revived}\n")
        );
    }

    #[test]
    fn resolve_id_reports_missing_and_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
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
        let (store, _fsck) = open_store(tmp.path());
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
        let (store, _fsck) = open_store(tmp.path());
        let note_id = note(&store, "n", None);
        let dataset_id = DatasetStore::from(&store).create().unwrap();

        let refs = store.list_object_refs().unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.ref_name.as_str()).collect();
        assert!(names.contains(&object_ref(&note_id).as_str()));
        assert!(names.contains(&object_ref(&dataset_id).as_str()));
    }

    #[test]
    fn classify_parents_splits_parent_and_links() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);

        let root = note(&store, "root", None);
        let root_commit = store.resolve_id(&root).unwrap().1;
        let child = note(&store, "reply", Some(&format!("note:{root}")));
        let first_child = store.resolve_id(&child).unwrap().1;
        notes
            .edit(&child, &NoteValue::Text("second".into()))
            .unwrap();

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
        let (store, _fsck) = open_store(tmp.path());
        let notes = NoteStore::from(&store);

        let id = note(&store, "n", None);
        notes.edit(&id, &NoteValue::Text("v2".into())).unwrap();
        notes.edit(&id, &NoteValue::Text("v3".into())).unwrap();

        let tip = store.resolve_id(&id).unwrap().1;
        let chain = store.walk_parent_chain(&tip).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], tip);
    }

    #[test]
    fn find_link_files_reads_target_link() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let root = note(&store, "root", None);
        let root_commit = store.resolve_id(&root).unwrap().1;
        let child = note(&store, "reply", Some(&format!("note:{root}")));
        let files = store.find_link_files(&object_ref(&child)).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "target.link");
        assert_eq!(files[0].shas, vec![root_commit]);
    }
}
