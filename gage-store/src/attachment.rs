//! Attachment objects: `gage::attachment 1`, reached through
//! [`AttachmentStore`].
//!
//! An attachment is a named file tree a user puts in the store so
//! scanners can read it: a harness config directory, a project's
//! manifests. Content is `attrs.json` (name, root, patterns, file
//! count, size) and the opaque `files.d/**` subtree holding the
//! selected files at their paths relative to the root. The object id
//! is derived from the name, so re-adding under a name updates the
//! same object: unchanged content writes nothing, changed content
//! writes an edit commit. A dataset links attachments through
//! `attachments.link` the way it links sessions. Tree construction,
//! commit parents, and edits are the generic object model's job; see
//! [`crate::object`].
//!
//! File selection follows ripgrep's `--glob`: gitignore syntax, an
//! ordered list where the last matching pattern wins, a leading `!`
//! excludes, and a set with only positive patterns selects those
//! files alone. An empty pattern list selects every file under the
//! root. Hidden files are included; `.gitignore` files are not
//! consulted.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gage_core::uuid::derive_id;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde::{Deserialize, Serialize};

use crate::dataset::{DatasetAttachments, DatasetStore};
use crate::git::EntryKind;
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::object::{EditOutcome, Object, ObjectTree, object_ref, require_type};
use crate::session::build_files_tree;
use crate::writer::write_blob;
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::attachment";
const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from an attachment's `attrs.json`.
pub(crate) const INDEXED_ATTRS: &[&str] = &["name"];
const FILES_TREE: &str = "files.d";

/// Attachment operations over an opened store.
pub struct AttachmentStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for AttachmentStore<'a> {
    fn from(store: &'a Store) -> Self {
        AttachmentStore { store }
    }
}

/// The `attrs.json` shape of an attachment.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AttachmentAttrs {
    pub name: String,
    /// The directory the files were selected under, on the machine
    /// that added them. File keys are paths relative to it.
    pub root: PathBuf,
    /// The selection patterns, in order. Empty selects every file.
    pub patterns: Vec<String>,
    pub file_count: u64,
    /// Total bytes across the selected files
    pub size: u64,
}

/// What to add: the name and the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSpec<'a> {
    pub name: &'a str,
    pub root: &'a Path,
    pub patterns: &'a [String],
}

/// Outcome of writing one attachment to the store.
#[derive(Debug, PartialEq, Eq)]
pub struct AttachmentAddOutcome {
    pub id: String,
    /// Commit SHA of the resulting version. When `outcome` is
    /// [`AttachmentOutcome::Unchanged`] this is the existing commit.
    pub commit_sha: String,
    pub outcome: AttachmentOutcome,
    pub file_count: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AttachmentOutcome {
    Added,
    /// The object existed and its content changed
    Updated,
    /// The object existed with the same content; no commit was written
    Unchanged,
}

/// Outcome of removing attachments from the store.
#[derive(Debug, PartialEq, Eq)]
pub struct AttachmentRemoveOutcome {
    /// The removed attachments as they were before removal, in
    /// argument order with duplicates dropped
    pub attachments: Vec<AttachmentRecord>,
    /// The datasets that held any of the attachments, each with the
    /// members unlinked from it
    pub datasets: Vec<DatasetAttachments>,
}

/// A stored attachment presented for reading, resolved to its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub id: String,
    pub commit_sha: String,
    /// UNIX time millis of the object's `created` marker
    pub created_ms: Option<i64>,
    /// UNIX time millis of the version's `modified` marker
    pub modified_ms: Option<i64>,
    pub attrs: AttachmentAttrs,
}

/// One file of an attachment version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentFile {
    /// The path relative to the root, `/`-separated
    pub key: String,
    pub size: u64,
}

/// Derive the Gage object id of an attachment from its name.
pub fn attachment_object_id(name: &str) -> String {
    derive_id(&format!("attachment\0{name}"))
}

impl AttachmentStore<'_> {
    /// Select the files under `spec.root` and write them as an
    /// attachment object. Idempotent when the selected files and
    /// their content are unchanged.
    pub fn add(&self, spec: &AttachmentSpec<'_>) -> Result<AttachmentAddOutcome, StoreError> {
        validate_name(spec.name)?;
        if !spec.root.is_dir() {
            return Err(StoreError::AttachmentInput(format!(
                "root {} is not a directory",
                spec.root.display()
            )));
        }
        let path = self.store.path();
        let id = attachment_object_id(spec.name);

        let mut entries: Vec<(String, String)> = Vec::new();
        let mut size = 0;
        for (key, file) in select_files(spec.root, spec.patterns)? {
            let bytes = fs::read(&file).map_err(|e| read_error(&file, e))?;
            size += bytes.len() as u64;
            entries.push((key, write_blob(path, &bytes)?));
        }
        let file_count = entries.len() as u64;
        let files_tree_sha = build_files_tree(path, entries)?;

        let attrs = AttachmentAttrs {
            name: spec.name.to_string(),
            root: spec.root.to_path_buf(),
            patterns: spec.patterns.to_vec(),
            file_count,
            size,
        };
        let mut tree = ObjectTree {
            attrs: Some(
                serde_json::to_value(&attrs)
                    .map_err(|e| StoreError::Parse(format!("attachment attrs encode: {e}")))?,
            ),
            ..ObjectTree::default()
        };
        tree.subtrees.insert(FILES_TREE.to_string(), files_tree_sha);

        match self.store.rev_parse(&object_ref(&id))? {
            None => {
                let message = format!("attachment: {}", spec.name);
                let commit_sha =
                    self.store
                        .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
                Ok(AttachmentAddOutcome {
                    id,
                    commit_sha,
                    outcome: AttachmentOutcome::Added,
                    file_count,
                })
            }
            Some(sha) => {
                let current = self.store.read_object(&sha)?;
                require_type(&current, OBJECT_TYPE)?;
                if current.header.is_tombstone() {
                    // Re-add after remove: the derived id names the
                    // same object, so a live commit supersedes the
                    // tombstone under the object's identity
                    let message = format!("attachment: {}", spec.name);
                    let commit_sha = self.store.resurrect(&current, &tree, &message)?;
                    return Ok(AttachmentAddOutcome {
                        id,
                        commit_sha,
                        outcome: AttachmentOutcome::Added,
                        file_count,
                    });
                }
                let message = format!("attachment edit: {}", spec.name);
                match self.store.edit(&current, &tree, &message)? {
                    EditOutcome::Unchanged => Ok(AttachmentAddOutcome {
                        id,
                        commit_sha: current.commit_sha,
                        outcome: AttachmentOutcome::Unchanged,
                        file_count,
                    }),
                    EditOutcome::Written(commit_sha) => Ok(AttachmentAddOutcome {
                        id,
                        commit_sha,
                        outcome: AttachmentOutcome::Updated,
                        file_count,
                    }),
                }
            }
        }
    }

    /// Remove stored attachments. Every id or prefix is resolved to a
    /// live attachment before anything is written. Each dataset that
    /// currently holds any of them is edited once to unlink all of
    /// them, then each attachment's ref gets a parentless tombstone
    /// commit.
    pub fn remove(
        &self,
        ids_or_prefixes: &[String],
    ) -> Result<AttachmentRemoveOutcome, StoreError> {
        let mut attachments: Vec<AttachmentRecord> = Vec::with_capacity(ids_or_prefixes.len());
        for id_or_prefix in ids_or_prefixes {
            let record = self.get(id_or_prefix)?;
            if !attachments.iter().any(|r| r.id == record.id) {
                attachments.push(record);
            }
        }
        let ids: Vec<String> = attachments.iter().map(|r| r.id.clone()).collect();

        let datasets = DatasetStore::from(self.store);
        let held = datasets.containing_attachments(&ids)?;
        for members in &held {
            datasets.attachments_unlink(&members.dataset_id, &members.attachment_ids)?;
        }

        for record in &attachments {
            let object = self.store.read_object(&record.commit_sha)?;
            let message = format!("attachment remove: {}", record.attrs.name);
            self.store.delete(&object, &message)?;
        }
        Ok(AttachmentRemoveOutcome {
            attachments,
            datasets: held,
        })
    }

    /// Read the live attachment for `id_or_prefix`.
    pub fn get(&self, id_or_prefix: &str) -> Result<AttachmentRecord, StoreError> {
        decode(self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?)
    }

    /// Read the live attachment named `name`.
    pub fn get_by_name(&self, name: &str) -> Result<AttachmentRecord, StoreError> {
        self.get(&attachment_object_id(name))
    }

    /// Every live attachment, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<AttachmentRecord, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over attachments.
    pub fn query(&self) -> AttachmentQuery<'_> {
        AttachmentQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// Read the attachment at the given commit SHA.
    pub fn at_commit(&self, commit_sha: &str) -> Result<AttachmentRecord, StoreError> {
        decode(self.store.read_object(commit_sha)?)
    }

    /// The files of the attachment version at `commit_sha`, in path
    /// order.
    pub fn files(&self, commit_sha: &str) -> Result<Vec<AttachmentFile>, StoreError> {
        let mut out = Vec::new();
        self.store
            .walk_tree(&format!("{commit_sha}:{FILES_TREE}"), "", &mut |entry| {
                if entry.kind == EntryKind::Blob {
                    out.push(AttachmentFile {
                        key: entry.name,
                        size: entry.size.unwrap_or(0),
                    });
                }
                Ok(())
            })?;
        Ok(out)
    }

    /// The bytes of the file `key` in the attachment version at
    /// `commit_sha`, or `None` when the version has no such file.
    pub fn read_file(&self, commit_sha: &str, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .store
            .object_contents(&format!("{commit_sha}:{FILES_TREE}/{key}"))?
            .map(|(_, bytes)| bytes))
    }
}

/// The files `patterns` select under `root`, as `(key, path)` pairs
/// in path order.
fn select_files(root: &Path, patterns: &[String]) -> Result<Vec<(String, PathBuf)>, StoreError> {
    let mut overrides = OverrideBuilder::new(root);
    for pattern in patterns {
        overrides
            .add(pattern)
            .map_err(|e| StoreError::AttachmentInput(format!("pattern {pattern:?}: {e}")))?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| StoreError::AttachmentInput(format!("patterns: {e}")))?;
    let walk = WalkBuilder::new(root)
        .standard_filters(false)
        .follow_links(true)
        .overrides(overrides)
        .sort_by_file_path(Path::cmp)
        .build();
    let mut out = Vec::new();
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            // A dangling symlink is not a file to select
            Err(e)
                if e.io_error()
                    .is_some_and(|io| io.kind() == io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(StoreError::AttachmentInput(format!("walk: {e}"))),
        };
        let path = entry.path();
        // Directories select nothing themselves; a symlink is read
        // through, so a link to a file is its target's file
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("the walk yields paths under its root");
        let mut key = String::new();
        for component in rel.components() {
            let text = component
                .as_os_str()
                .to_str()
                .ok_or_else(|| StoreError::InvalidPath {
                    path: rel.display().to_string(),
                    reason: "path is not UTF-8".to_string(),
                })?;
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(text);
        }
        out.push((key, path.to_path_buf()));
    }
    Ok(out)
}

fn validate_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::AttachmentInput("name is empty".to_string()));
    }
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid || name.starts_with('.') {
        return Err(StoreError::AttachmentInput(format!(
            "name {name:?}: use letters, digits, `-`, `_`, and `.`, not starting with `.`"
        )));
    }
    Ok(())
}

fn read_error(path: &Path, e: io::Error) -> StoreError {
    StoreError::Read {
        path: path.to_path_buf(),
        source: e,
    }
}

/// A selection over attachments: a name filter, an order, and a
/// limit. `iter` reads matching attachments one at a time.
pub struct AttachmentQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> AttachmentQuery<'a> {
    /// Select the attachment named `name`.
    pub fn name(mut self, name: &str) -> Self {
        self.query.attrs.push(("name", name.to_string()));
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
    /// one step; each attachment is read from the repository as the
    /// iterator advances.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<AttachmentRecord, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let tips = store.select(&self.query)?;
        Ok(tips
            .into_iter()
            .map(move |tip| decode(store.read_object(&tip.sha)?)))
    }

    /// The matching tips, in query order, without reading any object.
    pub fn tips(self) -> Result<Vec<SelectedTip>, StoreError> {
        self.store.select(&self.query)
    }
}

fn decode(object: Object) -> Result<AttachmentRecord, StoreError> {
    let commit_sha = object.commit_sha.as_str();
    require_type(&object, OBJECT_TYPE)?;
    let attrs_value = object
        .tree
        .attrs
        .ok_or_else(|| StoreError::Parse(format!("attachment {commit_sha}: missing attrs.json")))?;
    let attrs: AttachmentAttrs = serde_json::from_value(attrs_value)
        .map_err(|e| StoreError::Parse(format!("attachment attrs {commit_sha}: {e}")))?;
    Ok(AttachmentRecord {
        id: object.header.id,
        commit_sha: object.commit_sha.clone(),
        created_ms: object.header.created_ms,
        modified_ms: object.header.modified_ms,
        attrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A config-shaped tree: settings at the root, a nested skill, a
    /// hidden file, and a bulky directory the patterns leave out.
    fn fixture_root(dir: &Path) -> PathBuf {
        let root = dir.join("claude");
        write(&root.join("settings.json"), "{\"a\":1}");
        write(&root.join("settings.local.json"), "{}");
        write(&root.join("CLAUDE.md"), "rules");
        write(&root.join(".hidden"), "h");
        write(&root.join("skills/rust/SKILL.md"), "skill");
        write(&root.join("skills/rust/node_modules/x.js"), "js");
        write(&root.join("projects/p/session.jsonl"), "{}");
        root
    }

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn keys(files: &[AttachmentFile]) -> Vec<&str> {
        files.iter().map(|f| f.key.as_str()).collect()
    }

    #[test]
    fn empty_patterns_select_every_file_including_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        let selected = select_files(&root, &[]).unwrap();
        let keys: Vec<&str> = selected.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                ".hidden",
                "CLAUDE.md",
                "projects/p/session.jsonl",
                "settings.json",
                "settings.local.json",
                "skills/rust/SKILL.md",
                "skills/rust/node_modules/x.js",
            ]
        );
    }

    #[test]
    fn positive_patterns_select_only_matches_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        let selected = select_files(&root, &patterns(&["settings*.json", "*.md"])).unwrap();
        let keys: Vec<&str> = selected.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "CLAUDE.md",
                "settings.json",
                "settings.local.json",
                "skills/rust/SKILL.md",
            ]
        );
    }

    #[test]
    fn a_negated_pattern_prunes_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        let selected = select_files(
            &root,
            &patterns(&["skills/**", "!node_modules", "!projects"]),
        )
        .unwrap();
        let keys: Vec<&str> = selected.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["skills/rust/SKILL.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();
        let selected = select_files(&root, &patterns(&["CLAUDE.md", "dangling"])).unwrap();
        let keys: Vec<&str> = selected.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["CLAUDE.md"]);
    }

    #[test]
    fn a_malformed_pattern_is_input_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        let err = select_files(&root, &patterns(&["a[b"])).unwrap_err();
        assert!(matches!(err, StoreError::AttachmentInput(_)), "{err}");
    }

    #[test]
    fn add_writes_files_and_reads_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let spec = AttachmentSpec {
            name: "claude-config",
            root: &root,
            patterns: &patterns(&["settings*.json"]),
        };
        let added = attachments.add(&spec).unwrap();
        assert_eq!(added.outcome, AttachmentOutcome::Added);
        assert_eq!(added.file_count, 2);
        assert_eq!(added.id, attachment_object_id("claude-config"));

        let record = attachments.get_by_name("claude-config").unwrap();
        assert_eq!(record.commit_sha, added.commit_sha);
        assert_eq!(record.attrs.name, "claude-config");
        assert_eq!(record.attrs.root, root);
        assert_eq!(record.attrs.patterns, ["settings*.json"]);
        assert_eq!(record.attrs.file_count, 2);
        assert_eq!(record.attrs.size, 9);

        let files = attachments.files(&record.commit_sha).unwrap();
        assert_eq!(keys(&files), ["settings.json", "settings.local.json"]);
        assert_eq!(files[0].size, 7);
        assert_eq!(
            attachments
                .read_file(&record.commit_sha, "settings.json")
                .unwrap(),
            Some(b"{\"a\":1}".to_vec())
        );
        assert_eq!(
            attachments.read_file(&record.commit_sha, "nope").unwrap(),
            None
        );
    }

    #[test]
    fn re_add_is_unchanged_then_updated_under_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let pats = patterns(&["settings.json"]);
        let spec = AttachmentSpec {
            name: "cfg",
            root: &root,
            patterns: &pats,
        };
        let first = attachments.add(&spec).unwrap();
        let again = attachments.add(&spec).unwrap();
        assert_eq!(again.outcome, AttachmentOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);

        write(&root.join("settings.json"), "{\"a\":2}");
        let updated = attachments.add(&spec).unwrap();
        assert_eq!(updated.outcome, AttachmentOutcome::Updated);
        assert_eq!(updated.id, first.id);
        assert_ne!(updated.commit_sha, first.commit_sha);
        let record = attachments.get(&first.id).unwrap();
        assert_eq!(
            attachments
                .read_file(&record.commit_sha, "settings.json")
                .unwrap(),
            Some(b"{\"a\":2}".to_vec())
        );
        assert_eq!(attachments.iter().unwrap().count(), 1);
    }

    #[test]
    fn query_by_name_and_remove_then_re_add() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let pats = patterns(&["CLAUDE.md"]);
        for name in ["a", "b"] {
            attachments
                .add(&AttachmentSpec {
                    name,
                    root: &root,
                    patterns: &pats,
                })
                .unwrap();
        }
        let found: Vec<AttachmentRecord> = attachments
            .query()
            .name("b")
            .iter()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].attrs.name, "b");

        let removed = attachments.remove(&["b".to_string()]).unwrap_err();
        assert!(
            matches!(removed, StoreError::ObjectNotFound(_)),
            "{removed}"
        );
        let id_b = attachment_object_id("b");
        let removed = attachments.remove(&[id_b.clone()]).unwrap();
        assert_eq!(removed.attachments[0].attrs.name, "b");
        assert!(removed.datasets.is_empty());
        assert!(matches!(
            attachments.get(&id_b).unwrap_err(),
            StoreError::ObjectDeleted(_)
        ));
        assert_eq!(attachments.iter().unwrap().count(), 1);

        let back = attachments
            .add(&AttachmentSpec {
                name: "b",
                root: &root,
                patterns: &pats,
            })
            .unwrap();
        assert_eq!(back.outcome, AttachmentOutcome::Added);
        assert_eq!(back.id, id_b);
        assert_eq!(attachments.iter().unwrap().count(), 2);
    }

    #[test]
    fn dataset_links_unlinks_and_reads_attachments() {
        use crate::dataset::AttachmentLinkOutcome;

        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let datasets = DatasetStore::from(&store);
        let pats = patterns(&["settings.json"]);
        let spec = AttachmentSpec {
            name: "cfg",
            root: &root,
            patterns: &pats,
        };
        let first = attachments.add(&spec).unwrap();
        let dataset = datasets.create().unwrap();

        let linked = datasets
            .attachments_link(&dataset, &["cfg".to_string()])
            .unwrap_err();
        assert!(matches!(linked, StoreError::ObjectNotFound(_)), "{linked}");
        let linked = datasets
            .attachments_link(&dataset, &[first.id.clone()])
            .unwrap();
        assert_eq!(linked[0].outcome, AttachmentLinkOutcome::Linked);
        assert_eq!(linked[0].name, "cfg");
        let record = datasets.get(&dataset).unwrap();
        assert_eq!(record.attachment_count, 1);
        let commit_after_link = record.commit_sha.clone();

        let read = datasets.attachments(&dataset).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].commit_sha, first.commit_sha);
        assert_eq!(
            datasets.attachments_at(&commit_after_link).unwrap()[0].id,
            first.id
        );

        // Linking again at the same commit writes nothing
        let again = datasets
            .attachments_link(&dataset, &[first.id.clone()])
            .unwrap();
        assert_eq!(again[0].outcome, AttachmentLinkOutcome::Unchanged);
        assert_eq!(
            datasets.get(&dataset).unwrap().commit_sha,
            commit_after_link
        );

        // A changed attachment advances the link; the old dataset
        // commit still reads the old attachment commit
        write(&root.join("settings.json"), "{\"a\":2}");
        let second = attachments.add(&spec).unwrap();
        let updated = datasets
            .attachments_link(&dataset, &[first.id.clone()])
            .unwrap();
        assert_eq!(updated[0].outcome, AttachmentLinkOutcome::Updated);
        assert_eq!(
            datasets.attachments(&dataset).unwrap()[0].commit_sha,
            second.commit_sha
        );
        assert_eq!(
            datasets.attachments_at(&commit_after_link).unwrap()[0].commit_sha,
            first.commit_sha
        );
        assert_eq!(
            datasets
                .containing_attachments(&[first.id.clone()])
                .unwrap(),
            vec![DatasetAttachments {
                dataset_id: dataset.clone(),
                attachment_ids: vec![first.id.clone()],
            }]
        );

        // Unlink keeps the object and drops the file
        let unlinked = datasets
            .attachments_unlink(&dataset, &[first.id.clone()])
            .unwrap();
        assert_eq!(unlinked[0].name, "cfg");
        assert_eq!(datasets.get(&dataset).unwrap().attachment_count, 0);
        assert!(datasets.attachments(&dataset).unwrap().is_empty());
        assert_eq!(
            attachments.get(&first.id).unwrap().commit_sha,
            second.commit_sha
        );
        let err = datasets
            .attachments_unlink(&dataset, &[first.id.clone()])
            .unwrap_err();
        assert!(matches!(err, StoreError::ObjectNotFound(_)), "{err}");
    }

    #[test]
    fn remove_unlinks_from_every_dataset() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let datasets = DatasetStore::from(&store);
        let pats = patterns(&["CLAUDE.md"]);
        let added = attachments
            .add(&AttachmentSpec {
                name: "cfg",
                root: &root,
                patterns: &pats,
            })
            .unwrap();
        let a = datasets.create().unwrap();
        let b = datasets.create().unwrap();
        for dataset in [&a, &b] {
            datasets
                .attachments_link(dataset, &[added.id.clone()])
                .unwrap();
        }
        let removed = attachments.remove(&[added.id.clone()]).unwrap();
        assert_eq!(removed.attachments.len(), 1);
        let mut held: Vec<&str> = removed
            .datasets
            .iter()
            .map(|d| d.dataset_id.as_str())
            .collect();
        held.sort();
        let mut expected = [a.as_str(), b.as_str()];
        expected.sort();
        assert_eq!(held, expected);
        for dataset in [&a, &b] {
            assert_eq!(datasets.get(dataset).unwrap().attachment_count, 0);
        }
    }

    #[test]
    fn bad_name_and_bad_root_are_input_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        for name in ["", "has space", ".dot", "a/b"] {
            let err = attachments
                .add(&AttachmentSpec {
                    name,
                    root: &root,
                    patterns: &[],
                })
                .unwrap_err();
            assert!(
                matches!(err, StoreError::AttachmentInput(_)),
                "{name:?}: {err}"
            );
        }
        let err = attachments
            .add(&AttachmentSpec {
                name: "ok",
                root: &root.join("settings.json"),
                patterns: &[],
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::AttachmentInput(_)), "{err}");
    }
}
