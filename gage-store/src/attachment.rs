//! Attachment objects: `gage::attachment 1`, reached through
//! [`AttachmentStore`].
//!
//! An attachment is a file tree put in the store for scanners to
//! read: a harness config directory, a project's manifests. Content
//! is `attrs.json` (optional name and key, targets, root, includes,
//! excludes, file count, size), the opaque `files.d/**` subtree
//! holding the selected files at their paths relative to the root,
//! and, when targets are given, `targets.link` naming each target
//! object's commit.
//!
//! A name is a selector, not an identifier; any number of attachments
//! may share one. The key is the identity a later add addresses. An
//! add whose key names a live attachment edits that attachment:
//! unchanged content and targets write nothing, anything else writes
//! an edit commit. An add whose key names nothing, or that has no
//! key, creates an attachment with a random id. A named add without
//! a key takes the default key: the name and the selection's key
//! part, colon-joined; see [`selection_key_part`]. Targets are the
//! objects the files are about, any number of them. An edit unions
//! the given targets with those already linked, and a target already
//! linked keeps the commit it was linked at. A dataset links
//! attachments through `attachments.link` the way it links sessions.
//! Tree construction, commit parents, and edits are the generic
//! object model's job; see [`crate::object`].
//!
//! File selection uses shell path globs, as Nushell's `glob` does:
//! an include is a path relative to the root, `*` and `?` do not
//! cross `/`, `**` does, and `{a,b}` and `[a-z]` are supported. A
//! bare `settings.json` names the one file at the root; any depth
//! needs `**/settings.json`. Excludes are path globs of the same
//! form; a directory an exclude matches is not descended. A pattern
//! that is absolute or contains `..` is refused. Symbolic links are
//! read through; a dangling link selects nothing.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gage_core::uuid::{new_uuid, short_uuid};
use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;
use wax::walk::{Entry, FileIterator, GlobEntry, LinkBehavior, WalkError};
use wax::{Glob, Program};

use crate::dataset::{DatasetAttachments, DatasetStore};
use crate::git::EntryKind;
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::key::{resolve_key, validate_key};
use crate::object::{EditOutcome, Object, ObjectTree, require_type, resolve_target};
use crate::session::build_files_tree;
use crate::url;
use crate::writer::write_blob;
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::attachment";
const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from an attachment's `attrs.json`.
pub(crate) const INDEXED_ATTRS: &[&str] = &["name", "key"];
const FILES_TREE: &str = "files.d";
const TARGETS_LINK: &str = "targets.link";
/// The most an attachment may hold
pub const MAX_SIZE: u64 = 10 * 1024 * 1024;
pub const MAX_FILES: usize = 1000;

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
    /// The selector scanners read the attachment by; absent for an
    /// unnamed attachment
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The identity a later add addresses; absent for an attachment
    /// no later add can address
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The objects the files are about, as Gage URLs with full ids,
    /// in `targets.link` order
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The directory the files were selected under, on the machine
    /// that added them. File keys are paths relative to it.
    pub root: PathBuf,
    /// The include patterns, path globs relative to the root
    pub includes: Vec<String>,
    /// The exclude patterns, path globs relative to the root
    pub excludes: Vec<String>,
    pub file_count: u64,
    /// Total bytes across the selected files
    pub size: u64,
    /// XxHash3 over the file keys and contents in key order: the
    /// identity of the files, unchanged by a name or target edit.
    /// Absent on attachments written before the digest existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// What to add: the selection and, optionally, the name, key, and
/// targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSpec<'a> {
    pub name: Option<&'a str>,
    /// The identity. `None` with a name takes the default key;
    /// `None` without one creates an attachment no later add can
    /// address.
    pub key: Option<&'a str>,
    /// Gage URLs of live objects with full ids and no fragments
    pub targets: &'a [String],
    pub root: &'a Path,
    /// At least one is required
    pub includes: &'a [String],
    pub excludes: &'a [String],
}

/// Outcome of writing one attachment to the store.
#[derive(Debug, PartialEq, Eq)]
pub struct AttachmentAddOutcome {
    pub id: String,
    /// The key the attachment is stored under, default filled in
    pub key: Option<String>,
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
    pub path: String,
    pub size: u64,
}

/// The part of the default key a selection contributes: a stable
/// 16-hex-digit XXH3 hash of the root and the patterns. It identifies
/// the selection, not the selected content, so the content may change
/// under the same key.
pub fn selection_key_part(root: &Path, includes: &[String], excludes: &[String]) -> String {
    let mut input = root.as_os_str().as_encoded_bytes().to_vec();
    for pattern in includes {
        input.extend_from_slice(b"\0i");
        input.extend_from_slice(pattern.as_bytes());
    }
    for pattern in excludes {
        input.extend_from_slice(b"\0x");
        input.extend_from_slice(pattern.as_bytes());
    }
    format!("{:016x}", XxHash3_64::oneshot(&input))
}

impl AttachmentRecord {
    /// The name, or the short id when unnamed, for messages
    pub fn label(&self) -> &str {
        self.attrs.name.as_deref().unwrap_or(short_uuid(&self.id))
    }
}

impl AttachmentStore<'_> {
    /// Select the files under `spec.root` and write them as an
    /// attachment object. A key that names a live attachment edits
    /// it, with the targets unioned; otherwise the add creates.
    pub fn add(&self, spec: &AttachmentSpec<'_>) -> Result<AttachmentAddOutcome, StoreError> {
        if let Some(name) = spec.name {
            validate_name(name)?;
        }
        if let Some(key) = spec.key {
            validate_key(key)?;
        }
        if !spec.root.is_dir() {
            return Err(StoreError::AttachmentInput(format!(
                "root {} is not a directory",
                spec.root.display()
            )));
        }
        let key = match (spec.key, spec.name) {
            (Some(key), _) => Some(key.to_string()),
            (None, Some(name)) => Some(format!(
                "{name}/{}",
                selection_key_part(spec.root, spec.includes, spec.excludes)
            )),
            (None, None) => None,
        };
        let current = match &key {
            Some(key) => self.live_under(key)?,
            None => None,
        };
        let mut targets = match &current {
            Some(current) => linked_targets(current)?,
            None => Vec::new(),
        };
        for raw in spec.targets {
            if targets.iter().any(|(url, _)| url == raw) {
                continue;
            }
            // An attachment is about an object, not a line range
            if url::parse(raw)?.fragment.is_some() {
                return Err(StoreError::BadTarget(raw.to_string()));
            }
            targets.push((raw.clone(), resolve_target(self.store, raw)?));
        }
        let path = self.store.path();

        let selected = select_files(spec.root, spec.includes, spec.excludes)?;
        if selected.len() > MAX_FILES {
            return Err(StoreError::AttachmentInput(format!(
                "{} files selected; an attachment holds at most {MAX_FILES}",
                selected.len()
            )));
        }
        let mut entries: Vec<(String, String)> = Vec::with_capacity(selected.len());
        let mut size = 0;
        let mut digest = ContentDigest::new();
        for (key, file) in selected {
            let bytes = fs::read(&file).map_err(|e| read_error(&file, e))?;
            size += bytes.len() as u64;
            digest.add(&key, &bytes);
            if size > MAX_SIZE {
                return Err(StoreError::AttachmentInput(format!(
                    "selected files exceed {} bytes; an attachment holds at most that",
                    MAX_SIZE
                )));
            }
            entries.push((key, write_blob(path, &bytes)?));
        }
        let file_count = entries.len() as u64;
        let files_tree_sha = build_files_tree(path, entries)?;

        let attrs = AttachmentAttrs {
            name: spec.name.map(String::from),
            key,
            targets: targets.iter().map(|(url, _)| url.clone()).collect(),
            root: spec.root.to_path_buf(),
            includes: spec.includes.to_vec(),
            excludes: spec.excludes.to_vec(),
            file_count,
            size,
            digest: Some(digest.finish()),
        };
        let mut tree = ObjectTree {
            attrs: Some(
                serde_json::to_value(&attrs)
                    .map_err(|e| StoreError::Parse(format!("attachment attrs encode: {e}")))?,
            ),
            ..ObjectTree::default()
        };
        tree.subtrees.insert(FILES_TREE.to_string(), files_tree_sha);
        if !targets.is_empty() {
            tree.links.insert(
                TARGETS_LINK.to_string(),
                targets.into_iter().map(|(_, sha)| sha).collect(),
            );
        }

        let label = spec.name.unwrap_or("unnamed");
        let Some(current) = current else {
            let id = new_uuid();
            let message = format!("attachment: {label}");
            let commit_sha =
                self.store
                    .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
            return Ok(AttachmentAddOutcome {
                id,
                key: attrs.key,
                commit_sha,
                outcome: AttachmentOutcome::Added,
                file_count,
            });
        };
        let message = format!("attachment edit: {label}");
        let id = current.header.id.clone();
        match self.store.edit(&current, &tree, &message)? {
            EditOutcome::Unchanged => Ok(AttachmentAddOutcome {
                id,
                key: attrs.key,
                commit_sha: current.commit_sha,
                outcome: AttachmentOutcome::Unchanged,
                file_count,
            }),
            EditOutcome::Written(commit_sha) => Ok(AttachmentAddOutcome {
                id,
                key: attrs.key,
                commit_sha,
                outcome: AttachmentOutcome::Updated,
                file_count,
            }),
        }
    }

    /// The live attachment under `key`, if any: one ref read through
    /// `refs/gage/1/key/attachment/<key>`.
    fn live_under(&self, key: &str) -> Result<Option<Object>, StoreError> {
        match resolve_key(self.store, OBJECT_TYPE, key)? {
            Some(object) => {
                require_type(&object, OBJECT_TYPE)?;
                Ok(Some(object))
            }
            None => Ok(None),
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
            let message = format!("attachment remove: {}", record.label());
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
                        path: entry.name,
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

/// The attachment's targets as `(url, commit)` pairs: the URLs from
/// `attrs.json` paired by position with the commits `targets.link`
/// names.
fn linked_targets(object: &Object) -> Result<Vec<(String, String)>, StoreError> {
    let attrs = decode_attrs(object)?;
    let shas = object
        .tree
        .links
        .get(TARGETS_LINK)
        .cloned()
        .unwrap_or_default();
    if attrs.targets.len() != shas.len() {
        return Err(StoreError::Parse(format!(
            "attachment {}: {} targets in attrs.json, {} in {TARGETS_LINK}",
            object.commit_sha,
            attrs.targets.len(),
            shas.len()
        )));
    }
    Ok(attrs.targets.into_iter().zip(shas).collect())
}

/// The files the includes select under `root`, less the excludes, as
/// `(key, path)` pairs in key order. Each include is walked on its
/// own and the results are merged, so a file two includes match is
/// listed once.
/// The content digest of an attachment version: XxHash3 over each
/// file's key and content hash, in key order. Files are hashed one
/// at a time so the input never holds a copy of the contents.
struct ContentDigest {
    input: Vec<u8>,
}

impl ContentDigest {
    fn new() -> Self {
        ContentDigest { input: Vec::new() }
    }

    fn add(&mut self, key: &str, bytes: &[u8]) {
        self.input.extend_from_slice(key.as_bytes());
        self.input.push(0);
        self.input
            .extend_from_slice(format!("{:016x}", XxHash3_64::oneshot(bytes)).as_bytes());
        self.input.push(0);
    }

    fn finish(self) -> String {
        format!("{:016x}", XxHash3_64::oneshot(&self.input))
    }
}

fn select_files(
    root: &Path,
    includes: &[String],
    excludes: &[String],
) -> Result<Vec<(String, PathBuf)>, StoreError> {
    if includes.is_empty() {
        return Err(StoreError::AttachmentInput(
            "at least one include pattern is required".to_string(),
        ));
    }
    // An exclude that matches a directory prunes it. Wax prunes only
    // on a pattern ending in `/**`, so each exclude is paired with
    // that form
    let excludes = excludes
        .iter()
        .flat_map(|p| {
            let subtree = (!p.ends_with("**")).then(|| format!("{}/**", p.trim_end_matches('/')));
            std::iter::once(p.clone()).chain(subtree)
        })
        .map(|p| parse_pattern(&p, "exclude"))
        .collect::<Result<Vec<_>, _>>()?;
    let mut out: BTreeMap<String, PathBuf> = BTreeMap::new();
    for include in includes {
        let glob = parse_pattern(include, "include")?;
        let walk = glob.walk_with_behavior(root, LinkBehavior::ReadTarget);
        if excludes.is_empty() {
            collect_files(root, walk, &mut out)?;
        } else {
            let not = wax::any(excludes.iter().cloned())
                .map_err(|e| StoreError::AttachmentInput(format!("excludes: {e}")))?;
            let walk = walk
                .not(not)
                .map_err(|e| StoreError::AttachmentInput(format!("excludes: {e}")))?;
            collect_files(root, walk, &mut out)?;
        }
    }
    Ok(out.into_iter().collect())
}

/// A path glob relative to the root: not absolute and without `..`.
fn parse_pattern(pattern: &str, what: &str) -> Result<Glob<'static>, StoreError> {
    let input =
        |reason: String| StoreError::AttachmentInput(format!("{what} {pattern:?}: {reason}"));
    if pattern.split('/').any(|c| c == "..") {
        return Err(input("`..` is not allowed".to_string()));
    }
    let glob = Glob::new(pattern).map_err(|e| input(e.to_string()))?;
    if !glob.has_root().is_never() {
        return Err(input(
            "an absolute pattern is not allowed; patterns are relative to the root".to_string(),
        ));
    }
    Ok(glob.into_owned())
}

/// Add every regular file the walk yields to `out`, keyed by its
/// `/`-separated path under `root`.
fn collect_files(
    root: &Path,
    walk: impl Iterator<Item = Result<GlobEntry, WalkError>>,
    out: &mut BTreeMap<String, PathBuf>,
) -> Result<(), StoreError> {
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let e = io::Error::from(e);
                // A dangling symlink is not a file to select
                if e.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                return Err(StoreError::AttachmentInput(format!("walk: {e}")));
            }
        };
        // Directories select nothing themselves; a link is read
        // through, so a link to a file is its target's file
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
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
        out.entry(key).or_insert_with(|| path.to_path_buf());
    }
    Ok(())
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

/// A selection over attachments: name and key filters, an order, and
/// a limit. `iter` reads matching attachments one at a time.
pub struct AttachmentQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> AttachmentQuery<'a> {
    /// Select the attachments named `name`.
    pub fn name(mut self, name: &str) -> Self {
        self.query.attrs.push(("name", name.to_string()));
        self
    }

    /// Select the attachments under `key`.
    pub fn key(mut self, key: &str) -> Self {
        self.query.attrs.push(("key", key.to_string()));
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
    require_type(&object, OBJECT_TYPE)?;
    let attrs = decode_attrs(&object)?;
    Ok(AttachmentRecord {
        id: object.header.id,
        commit_sha: object.commit_sha.clone(),
        created_ms: object.header.created_ms,
        modified_ms: object.header.modified_ms,
        attrs,
    })
}

fn decode_attrs(object: &Object) -> Result<AttachmentAttrs, StoreError> {
    let commit_sha = object.commit_sha.as_str();
    let attrs_value =
        object.tree.attrs.as_ref().ok_or_else(|| {
            StoreError::Parse(format!("attachment {commit_sha}: missing attrs.json"))
        })?;
    serde_json::from_value(attrs_value.clone())
        .map_err(|e| StoreError::Parse(format!("attachment attrs {commit_sha}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::object_ref;
    use crate::test_support::open_store;
    use std::slice;

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

    fn paths(files: &[AttachmentFile]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    fn selected_keys(root: &Path, includes: &[&str], excludes: &[&str]) -> Vec<String> {
        select_files(root, &patterns(includes), &patterns(excludes))
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    #[test]
    fn a_bare_name_selects_only_the_root_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        assert_eq!(
            selected_keys(&root, &["settings*.json", "*.md"], &[]),
            ["CLAUDE.md", "settings.json", "settings.local.json"]
        );
    }

    #[test]
    fn a_tree_wildcard_selects_at_any_depth_including_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        assert_eq!(
            selected_keys(&root, &["**/*"], &[]),
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
        assert_eq!(
            selected_keys(&root, &["**/*.md", "settings.json"], &[]),
            ["CLAUDE.md", "settings.json", "skills/rust/SKILL.md"]
        );
    }

    #[test]
    fn excludes_prune_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        assert_eq!(
            selected_keys(&root, &["**/*"], &["**/node_modules/**", "projects/**"]),
            [
                ".hidden",
                "CLAUDE.md",
                "settings.json",
                "settings.local.json",
                "skills/rust/SKILL.md",
            ]
        );
        // A bare directory name or glob prunes the directory too
        assert_eq!(
            selected_keys(&root, &["**/*"], &["proj*", "skills", "**/*.json"]),
            [".hidden", "CLAUDE.md"]
        );
    }

    #[test]
    fn a_file_matched_by_two_includes_is_listed_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        assert_eq!(
            selected_keys(&root, &["*.md", "CLAUDE.md"], &[]),
            ["CLAUDE.md"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_skipped_and_a_file_link_is_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();
        std::os::unix::fs::symlink(root.join("CLAUDE.md"), root.join("linked.md")).unwrap();
        assert_eq!(
            selected_keys(&root, &["CLAUDE.md", "dangling", "linked.md"], &[]),
            ["CLAUDE.md", "linked.md"]
        );
    }

    #[test]
    fn bad_patterns_are_input_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = fixture_root(dir.path());
        for (includes, excludes) in [
            (vec![], vec![]),
            (vec!["a[b"], vec![]),
            (vec!["/settings.json"], vec![]),
            (vec!["../x"], vec![]),
            (vec!["*"], vec!["/tmp/**"]),
        ] {
            let err = select_files(&root, &patterns(&includes), &patterns(&excludes)).unwrap_err();
            assert!(
                matches!(err, StoreError::AttachmentInput(_)),
                "{includes:?} {excludes:?}: {err}"
            );
        }
    }

    #[test]
    fn add_writes_files_and_reads_them_back() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let spec = AttachmentSpec {
            name: Some("claude-config"),
            key: None,
            targets: &[],
            root: &root,
            includes: &patterns(&["settings*.json"]),
            excludes: &[],
        };
        let added = attachments.add(&spec).unwrap();
        assert_eq!(added.outcome, AttachmentOutcome::Added);
        assert_eq!(added.file_count, 2);

        let record = attachments.get(&added.id).unwrap();
        assert_eq!(record.commit_sha, added.commit_sha);
        assert_eq!(record.attrs.name.as_deref(), Some("claude-config"));
        assert_eq!(
            record.attrs.key,
            Some(format!(
                "claude-config/{}",
                selection_key_part(&root, &patterns(&["settings*.json"]), &[])
            ))
        );
        assert!(record.attrs.targets.is_empty());
        assert_eq!(record.attrs.root, root);
        assert_eq!(record.attrs.includes, ["settings*.json"]);
        assert!(record.attrs.excludes.is_empty());
        assert_eq!(record.attrs.file_count, 2);
        assert_eq!(record.attrs.size, 9);

        let files = attachments.files(&record.commit_sha).unwrap();
        assert_eq!(paths(&files), ["settings.json", "settings.local.json"]);
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
            name: Some("cfg"),
            key: None,
            targets: &[],
            root: &root,
            includes: &pats,
            excludes: &[],
        };
        let first = attachments.add(&spec).unwrap();
        let first_digest = attachments.get(&first.id).unwrap().attrs.digest;
        assert_eq!(first_digest.as_ref().map(String::len), Some(16));
        let again = attachments.add(&spec).unwrap();
        assert_eq!(again.outcome, AttachmentOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);

        write(&root.join("settings.json"), "{\"a\":2}");
        let updated = attachments.add(&spec).unwrap();
        assert_eq!(updated.outcome, AttachmentOutcome::Updated);
        assert_eq!(updated.id, first.id);
        assert_ne!(updated.commit_sha, first.commit_sha);
        let record = attachments.get(&first.id).unwrap();
        assert!(record.attrs.digest.is_some());
        assert_ne!(record.attrs.digest, first_digest);
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
                    name: Some(name),
                    key: None,
                    targets: &[],
                    root: &root,
                    includes: &pats,
                    excludes: &[],
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
        assert_eq!(found[0].attrs.name.as_deref(), Some("b"));

        let removed = attachments.remove(&["b".to_string()]).unwrap_err();
        assert!(
            matches!(removed, StoreError::ObjectNotFound(_)),
            "{removed}"
        );
        let id_b = found[0].id.clone();
        let removed = attachments.remove(slice::from_ref(&id_b)).unwrap();
        assert_eq!(removed.attachments[0].attrs.name.as_deref(), Some("b"));
        assert!(removed.datasets.is_empty());
        assert!(matches!(
            attachments.get(&id_b).unwrap_err(),
            StoreError::ObjectDeleted(_)
        ));
        assert_eq!(attachments.iter().unwrap().count(), 1);

        let back = attachments
            .add(&AttachmentSpec {
                name: Some("b"),
                key: None,
                targets: &[],
                root: &root,
                includes: &pats,
                excludes: &[],
            })
            .unwrap();
        assert_eq!(back.outcome, AttachmentOutcome::Added);
        assert_ne!(back.id, id_b, "a removed attachment is not re-addressed");
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
            name: Some("cfg"),
            key: None,
            targets: &[],
            root: &root,
            includes: &pats,
            excludes: &[],
        };
        let first = attachments.add(&spec).unwrap();
        let dataset = datasets.create().unwrap();

        let linked = datasets
            .attachments_link(&dataset, &["cfg".to_string()])
            .unwrap_err();
        assert!(matches!(linked, StoreError::ObjectNotFound(_)), "{linked}");
        let linked = datasets
            .attachments_link(&dataset, slice::from_ref(&first.id))
            .unwrap();
        assert_eq!(linked[0].outcome, AttachmentLinkOutcome::Linked);
        assert_eq!(linked[0].label, "cfg");
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
            .attachments_link(&dataset, slice::from_ref(&first.id))
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
            .attachments_link(&dataset, slice::from_ref(&first.id))
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
                .containing_attachments(slice::from_ref(&first.id))
                .unwrap(),
            vec![DatasetAttachments {
                dataset_id: dataset.clone(),
                attachment_ids: vec![first.id.clone()],
            }]
        );

        // Unlink keeps the object and drops the file
        let unlinked = datasets
            .attachments_unlink(&dataset, slice::from_ref(&first.id))
            .unwrap();
        assert_eq!(unlinked[0].label, "cfg");
        assert_eq!(datasets.get(&dataset).unwrap().attachment_count, 0);
        assert!(datasets.attachments(&dataset).unwrap().is_empty());
        assert_eq!(
            attachments.get(&first.id).unwrap().commit_sha,
            second.commit_sha
        );
        let err = datasets
            .attachments_unlink(&dataset, slice::from_ref(&first.id))
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
                name: Some("cfg"),
                key: None,
                targets: &[],
                root: &root,
                includes: &pats,
                excludes: &[],
            })
            .unwrap();
        let a = datasets.create().unwrap();
        let b = datasets.create().unwrap();
        for dataset in [&a, &b] {
            datasets
                .attachments_link(dataset, slice::from_ref(&added.id))
                .unwrap();
        }
        let removed = attachments.remove(slice::from_ref(&added.id)).unwrap();
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
                    name: Some(name),
                    key: None,
                    targets: &[],
                    root: &root,
                    includes: &patterns(&["CLAUDE.md"]),
                    excludes: &[],
                })
                .unwrap_err();
            assert!(
                matches!(err, StoreError::AttachmentInput(_)),
                "{name:?}: {err}"
            );
        }
        let err = attachments
            .add(&AttachmentSpec {
                name: Some("ok"),
                key: None,
                targets: &[],
                root: &root.join("settings.json"),
                includes: &patterns(&["CLAUDE.md"]),
                excludes: &[],
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::AttachmentInput(_)), "{err}");
    }

    #[test]
    fn an_unnamed_add_creates_a_new_attachment_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let pats = patterns(&["settings.json"]);
        let spec = AttachmentSpec {
            name: None,
            key: None,
            targets: &[],
            root: &root,
            includes: &pats,
            excludes: &[],
        };
        let first = attachments.add(&spec).unwrap();
        let second = attachments.add(&spec).unwrap();
        assert_eq!(first.outcome, AttachmentOutcome::Added);
        assert_eq!(second.outcome, AttachmentOutcome::Added);
        assert_ne!(first.id, second.id);
        let record = attachments.get(&first.id).unwrap();
        assert_eq!(record.attrs.name, None);
        assert_eq!(record.label(), short_uuid(&first.id));
        assert_eq!(attachments.iter().unwrap().count(), 2);
        assert_eq!(attachments.query().name("x").iter().unwrap().count(), 0);
    }

    #[test]
    fn a_targeted_add_links_the_targets_and_a_re_add_unions_them() {
        use crate::SessionStore;
        use crate::session::tests::{FakeDriver, fake};

        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let session = SessionStore::from(&store)
            .add(&FakeDriver, &mut fake("s1", &[("log.jsonl", "{}")]))
            .unwrap();
        let target = format!("session:{}", session.id);
        let pats = patterns(&["CLAUDE.md"]);
        let spec = AttachmentSpec {
            name: Some("stack-files"),
            key: None,
            targets: std::slice::from_ref(&target),
            root: &root,
            includes: &pats,
            excludes: &[],
        };
        let added = attachments.add(&spec).unwrap();
        let object = store.read_object(&added.commit_sha).unwrap();
        assert_eq!(
            object.tree.links.get("targets.link"),
            Some(&vec![session.commit_sha.clone()])
        );
        let parents = store.read_commit(&added.commit_sha).unwrap().parents;
        assert!(parents.contains(&session.commit_sha));
        let record = attachments.get(&added.id).unwrap();
        assert_eq!(record.attrs.targets, slice::from_ref(&target));
        let digest = record.attrs.digest.clone();
        assert!(digest.is_some());

        // The same name and selection is the same attachment; a
        // re-add without targets keeps the linked one
        let untargeted = attachments
            .add(&AttachmentSpec {
                targets: &[],
                ..spec.clone()
            })
            .unwrap();
        assert_eq!(untargeted.outcome, AttachmentOutcome::Unchanged);
        assert_eq!(untargeted.id, added.id);
        assert_eq!(
            attachments
                .query()
                .name("stack-files")
                .iter()
                .unwrap()
                .count(),
            1
        );

        // A new target is unioned in; any live object is a target
        let dataset = DatasetStore::from(&store).create().unwrap();
        let dataset_url = format!("dataset:{dataset}");
        let on_dataset = attachments
            .add(&AttachmentSpec {
                targets: slice::from_ref(&dataset_url),
                ..spec.clone()
            })
            .unwrap();
        assert_eq!(on_dataset.outcome, AttachmentOutcome::Updated);
        assert_eq!(on_dataset.id, added.id);
        let tip = store.rev_parse(&object_ref(&dataset)).unwrap().unwrap();
        let object = store.read_object(&on_dataset.commit_sha).unwrap();
        assert_eq!(
            object.tree.links.get("targets.link"),
            Some(&vec![session.commit_sha.clone(), tip])
        );
        let record = attachments.get(&added.id).unwrap();
        assert_eq!(record.attrs.targets, [target.clone(), dataset_url]);
        // A target edit is a new commit with the same digest
        assert_eq!(record.attrs.digest, digest);

        // Re-adding the original spec is unchanged: its target is
        // already linked
        assert_eq!(
            attachments.add(&spec).unwrap().outcome,
            AttachmentOutcome::Unchanged
        );
    }

    #[test]
    fn an_explicit_key_addresses_the_attachment_across_names_and_roots() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let pats = patterns(&["CLAUDE.md"]);
        let keyed = attachments
            .add(&AttachmentSpec {
                name: Some("a"),
                key: Some("project/x"),
                targets: &[],
                root: &root,
                includes: &pats,
                excludes: &[],
            })
            .unwrap();
        assert_eq!(keyed.outcome, AttachmentOutcome::Added);
        let renamed = attachments
            .add(&AttachmentSpec {
                name: Some("b"),
                key: Some("project/x"),
                targets: &[],
                root: &root.join("skills"),
                includes: &patterns(&["**/*.md"]),
                excludes: &[],
            })
            .unwrap();
        assert_eq!(renamed.outcome, AttachmentOutcome::Updated);
        assert_eq!(renamed.id, keyed.id);
        let record = attachments.get(&keyed.id).unwrap();
        assert_eq!(record.attrs.name.as_deref(), Some("b"));
        assert_eq!(record.attrs.key.as_deref(), Some("project/x"));
        assert_eq!(
            attachments.query().key("project/x").tips().unwrap().len(),
            1,
            "the key is indexed"
        );
        for key in ["", "Upper", "..", "a b"] {
            let err = attachments
                .add(&AttachmentSpec {
                    name: None,
                    key: Some(key),
                    targets: &[],
                    root: &root,
                    includes: &pats,
                    excludes: &[],
                })
                .unwrap_err();
            assert!(matches!(err, StoreError::KeyName(_)), "{key:?}: {err}");
        }
    }

    #[test]
    fn a_bad_target_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let root = fixture_root(dir.path());
        let attachments = AttachmentStore::from(&store);
        let dataset = DatasetStore::from(&store).create().unwrap();
        let pats = patterns(&["CLAUDE.md"]);
        let add = |target: &str| {
            attachments
                .add(&AttachmentSpec {
                    name: Some("cfg"),
                    key: None,
                    targets: &[target.to_string()],
                    root: &root,
                    includes: &pats,
                    excludes: &[],
                })
                .unwrap_err()
        };
        let missing = add("session:00000000000000000000000000");
        assert!(
            matches!(missing, StoreError::TargetNotFound(_)),
            "{missing:?}"
        );
        let wrong_type = add(&format!("note:{dataset}"));
        assert!(
            matches!(wrong_type, StoreError::WrongType { .. }),
            "{wrong_type:?}"
        );
        let fragment = add("session:00000000000000000000000000#1-2");
        assert!(matches!(fragment, StoreError::BadTarget(_)), "{fragment:?}");
    }

    #[test]
    fn the_file_count_and_size_caps_are_input_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let attachments = AttachmentStore::from(&store);
        let many = dir.path().join("many");
        for i in 0..=MAX_FILES {
            write(&many.join(format!("f{i}")), "x");
        }
        let err = attachments
            .add(&AttachmentSpec {
                name: None,
                key: None,
                targets: &[],
                root: &many,
                includes: &patterns(&["*"]),
                excludes: &[],
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::AttachmentInput(_)), "{err}");

        let big = dir.path().join("big");
        fs::create_dir_all(&big).unwrap();
        fs::write(big.join("blob"), vec![0u8; MAX_SIZE as usize + 1]).unwrap();
        let err = attachments
            .add(&AttachmentSpec {
                name: None,
                key: None,
                targets: &[],
                root: &big,
                includes: &patterns(&["blob"]),
                excludes: &[],
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::AttachmentInput(_)), "{err}");
    }
}
