//! Session objects: `gage::session 1`, reached through [`SessionStore`].
//!
//! Content is `attrs.json` (driver, native session id, session type,
//! content format), the opaque `files.d/**` subtree holding the
//! session content as provided by the driver, and `changes.json`, the
//! records of what a derived session left out of its original. A
//! session added from a native source has an id derived from
//! `(driver_name, native_session_id)`, so the same native session
//! maps to the same object over time; adding one whose content
//! matches the stored version writes nothing, and changed content
//! writes an edit commit. A session created from a directory takes
//! the directory's name as its id and its tree as it stands. Tree
//! construction, commit parents, and edits are the generic object
//! model's job; see [`crate::object`].

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gage_core::uuid::{derive_id, is_id};
use gage_session::{ContentSink, ContentSource, Driver, NativeSession, SummaryAttrs};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::content::GitContentSource;
use crate::dataset::{DatasetMembers, DatasetStore};
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::object::{EditOutcome, Object, ObjectTree, object_ref, require_type};
use crate::scan::import_opaque_tree;
use crate::share::{ShareMembers, ShareStore};
use crate::writer::{TreeInput, blob_sha, is_dot_git, mktree, tree_sha, write_hashed_blob};
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::session";
pub(crate) const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from a session's `attrs.json`.
pub(crate) const INDEXED_ATTRS: &[&str] = &["summary.model"];
/// Top-level subtree name for the session's opaque file content. The
/// `.d` suffix marks the subtree as producer-owned; Gage schema
/// walkers do not descend into it.
pub(crate) const FILES_TREE: &str = "files.d";
const ATTRS_FILE: &str = "attrs.json";
pub(crate) const CHANGES_FILE: &str = "changes.json";
/// The session object's tree under a session directory
pub const OBJECT_DIR: &str = "session";

/// Session operations over an opened store.
pub struct SessionStore<'a> {
    store: &'a Store,
    max_session_bytes: Option<u64>,
    origin: Option<String>,
}

impl<'a> From<&'a Store> for SessionStore<'a> {
    fn from(store: &'a Store) -> Self {
        SessionStore {
            store,
            max_session_bytes: None,
            origin: None,
        }
    }
}

/// The `attrs.json` shape of a session.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SessionAttrsRecord {
    /// `"<driver name> <driver version>"`.
    pub driver: String,
    /// The session this one was made from, as a URL. A native origin
    /// is the session source URL the driver spelled,
    /// `<scheme>:<body>`, with the native id as its last segment; a
    /// derived origin is `session:<id>@<commit>`.
    pub origin: String,
    /// UNIX time millis: when the native artifact was last touched at
    /// its source, as the driver reported it. A recorded fact; `add`
    /// does not read it.
    pub native_mtime: i64,
    /// The size in bytes of the native artifact, as the driver
    /// reported it. A recorded fact; `add` does not read it. Not the
    /// stored content size; see `size`.
    pub native_size: u64,
    /// Harness family, e.g. `"claude"`. A category with no version.
    pub session_type: String,
    /// The driver's byte-layout string, as returned by
    /// [`Driver::write_native`].
    pub content_format: String,
    /// The total byte size of the content stored under `files.d`.
    pub size: u64,
    /// The project the session belongs to, as the driver names it.
    /// Absent when the harness has no such notion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The driver's projection of the session, written at add time.
    pub summary: SummaryAttrs,
    /// What established a published session. Absent on a session
    /// added from a native source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<PublishAttrs>,
}

/// The record a publish leaves in the derived session's `attrs.json`.
/// A share reads it to decide membership.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublishAttrs {
    /// The filters the publish applied, each at the version its
    /// definition declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<PublishFilter>,
}

/// One filter a publish applied.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublishFilter {
    pub name: String,
    /// The filter definition's semver `version`. Absent when the
    /// definition declared none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl SessionAttrsRecord {
    /// The native id, the identifying part of `origin`: its last path
    /// segment, or the scheme body when there is no path. For display
    /// and diagnostics, not an authoritative driver split.
    pub fn native_id(&self) -> &str {
        if let Some(i) = self.origin.rfind('/') {
            &self.origin[i + 1..]
        } else if let Some(i) = self.origin.find(':') {
            &self.origin[i + 1..]
        } else {
            &self.origin
        }
    }
}

/// UNIX time millis of `t`; a time before the epoch is zero.
fn system_time_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Outcome of writing one session to the store.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionAddOutcome {
    /// Derived Gage object id for the session.
    pub id: String,
    /// Commit SHA of the resulting version. When `outcome` is
    /// [`SessionOutcome::Unchanged`] and [`SessionOutcome::Older`]
    /// this is the existing commit.
    pub commit_sha: String,
    pub outcome: SessionOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    /// New session object created.
    Added,
    /// Existing session object updated with new content.
    Updated,
    /// Existing session content matched; no commit was written.
    Unchanged,
    /// The native content differs and the driver orders it before the
    /// stored state; the stored session was kept and no commit was
    /// written.
    Older,
}

/// Outcome of removing sessions from the store.
#[derive(Debug, PartialEq)]
pub struct SessionRemoveOutcome {
    /// The removed sessions as they were before removal, in argument
    /// order with duplicates dropped.
    pub sessions: Vec<SessionRecord>,
    /// The datasets that held any of the sessions, each with the
    /// members unlinked from it.
    pub datasets: Vec<DatasetMembers>,
    /// The shares that held any of the sessions, each with the members
    /// unlinked from it.
    pub shares: Vec<ShareMembers>,
}

/// A stored session presented for reading, resolved to its commit.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub commit_sha: String,
    /// The `<version>` half of the `type` blob at this commit
    pub type_version: String,
    /// UNIX time millis of the object's `created` marker
    pub created_ms: Option<i64>,
    /// UNIX time millis of the version's `modified` marker
    pub modified_ms: Option<i64>,
    pub attrs: SessionAttrsRecord,
    pub driver_name: String,
    pub driver_version: String,
}

/// Derive the Gage object id of a session from its driver name and
/// native session id.
pub fn session_object_id(driver_name: &str, native_session_id: &str) -> String {
    derive_id(&format!("session\0{driver_name}\0{native_session_id}"))
}

impl<'a> SessionStore<'a> {
    /// Cap the total content one `add` may write to the store. A
    /// session whose stored files exceed `max` bytes fails with
    /// [`StoreError::SessionTooLarge`] and writes no object. Without
    /// this, `add` stores content of any size.
    ///
    /// # Future storage selection
    ///
    /// Today a driver stores only the session's own transcript. Two
    /// extensions are planned around this cap. A scanner definition
    /// will declare what extra content it wants --- subagent
    /// transcripts, sidecar files matching a path or type --- and the
    /// scan command will refresh session records to include what the
    /// selected scanners need, subject to this same limit. `gage
    /// session add` will grow storage-selection options
    /// (`--store-subagents`, `--store-attachments`, `--store-all`).
    /// Because sessions are re-added over time and git shares identical
    /// blobs, content withheld today can be added later at no
    /// duplication cost.
    pub fn with_max_session_size(mut self, max: u64) -> Self {
        self.max_session_bytes = Some(max);
        self
    }

    /// Record `origin` as the session's `origin` in place of what the
    /// native session reports as its source. For a session whose only
    /// home was a scan task's agent run, the runtime names the task
    /// here, since the place the driver read it from does not outlive
    /// the scan.
    pub fn with_origin(mut self, origin: String) -> Self {
        self.origin = Some(origin);
        self
    }

    /// Write `session`'s content as a session object. `driver` drives
    /// the serialization through [`Driver::write_native`]; the
    /// returned `content_format` is stored as the session's
    /// `content_format`, and the attributes it derived from the bytes
    /// written are the session's attrs. The content is always read
    /// through the sink: its tree SHA is the identity the decision
    /// rests on. Content equal to the live object's, written by the
    /// same driver version in the same format, is unchanged. Content
    /// that differs is written when the driver orders it at or after
    /// the stored state, and reported as older and left alone when it
    /// orders it before. The
    /// store receives the content only when the session is written.
    pub fn add(
        &self,
        driver: &dyn Driver,
        session: &mut dyn NativeSession,
    ) -> Result<SessionAddOutcome, StoreError> {
        let path = self.store.path();
        let native_id = session.id().to_string();
        let origin = self
            .origin
            .clone()
            .unwrap_or_else(|| session.source().to_string());
        let id = session_object_id(driver.name(), &native_id);
        let session_type = session.session_type().to_string();
        let driver_stamp = format!("{} {}", driver.name(), driver.version());

        let current = match self.store.rev_parse(&object_ref(&id))? {
            None => None,
            Some(sha) => {
                let current = self.store.read_object(&sha)?;
                require_type(&current, OBJECT_TYPE)?;
                Some(current)
            }
        };

        let mut sink = GitContentSink::new(self.max_session_bytes);
        let written = match driver.write_native(session, &mut sink) {
            Ok(written) => written,
            // A size-cap abort surfaces through the sink, not as the
            // driver's own error, so report it as such.
            Err(e) => {
                return Err(sink
                    .take_write_error()
                    .unwrap_or_else(|| StoreError::Parse(format!("driver: {e}"))));
            }
        };
        let size = sink.total_bytes;
        let files = sink.into_files()?;
        let file_entries = || {
            files
                .iter()
                .map(|f| (f.path.clone(), f.sha.clone()))
                .collect()
        };
        let files_tree_sha = files_tree_sha(file_entries())?;

        // The live object, with whether it holds these same bytes
        let live = current
            .as_ref()
            .filter(|c| !c.header.is_tombstone())
            .map(|c| (c, c.tree.subtrees.get(FILES_TREE) == Some(&files_tree_sha)));
        let kept = |current: &Object, outcome| SessionAddOutcome {
            id: id.clone(),
            commit_sha: current.commit_sha.clone(),
            outcome,
        };
        let stored = match live {
            Some((current, same_content)) => {
                let stored = decode_attrs(current)?;
                // Same bytes are rewritten only when the driver or the
                // format that derived their attrs changed.
                if same_content
                    && stored.driver == driver_stamp
                    && stored.content_format == written.content_format
                {
                    return Ok(kept(current, SessionOutcome::Unchanged));
                }
                Some((current, same_content, stored))
            }
            None => None,
        };

        let derived = (written.attrs)().map_err(|e| StoreError::Parse(format!("driver: {e}")))?;
        let summary = SummaryAttrs::from_attrs(derived.as_ref());
        if let Some((current, false, stored)) = &stored
            && driver.native_order(&summary, &stored.summary) == Ordering::Less
        {
            return Ok(kept(current, SessionOutcome::Older));
        }

        let attrs = SessionAttrsRecord {
            driver: driver_stamp,
            origin,
            native_mtime: system_time_ms(derived.native_mtime()),
            native_size: derived.native_size(),
            session_type,
            content_format: written.content_format,
            size,
            project: derived.project_name().map(String::from),
            summary,
            publish: None,
        };

        // Content the live object holds is in the store already, loose
        // or packed, and is not written a second time.
        if !matches!(stored, Some((_, true, _))) {
            for file in &files {
                write_hashed_blob(path, &file.sha, &file.bytes)?;
            }
            build_files_tree_inner(path, file_entries())?;
        }
        let mut tree = ObjectTree {
            attrs: Some(
                serde_json::to_value(&attrs)
                    .map_err(|e| StoreError::Parse(format!("session attrs encode: {e}")))?,
            ),
            ..ObjectTree::default()
        };
        tree.subtrees.insert(FILES_TREE.to_string(), files_tree_sha);

        let subject = format!("{}:{}", driver.name(), native_id);
        match current {
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
            Some(current) => {
                if current.header.is_tombstone() {
                    // Re-add after remove: the derived id names the
                    // same object, so a live commit supersedes the
                    // tombstone under the object's identity
                    let message = format!("session: {subject}");
                    let commit_sha = self.store.resurrect(&current, &tree, &message)?;
                    return Ok(SessionAddOutcome {
                        id,
                        commit_sha,
                        outcome: SessionOutcome::Added,
                    });
                }
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

    /// Write the session under `dir`, a session directory `<id>/`: the
    /// directory name is the session's id and the `session/` tree is
    /// the object's content as it stands, `attrs.json` as the attrs,
    /// `changes.json` as a root blob, and `files.d` as the content
    /// subtree. The attrs and the records are checked to decode; the
    /// content is not inspected. The object's ref must not exist.
    /// Returns the commit SHA.
    pub fn create_from_dir(&self, dir: &Path) -> Result<String, StoreError> {
        let layout = SessionDirLayout::new(dir);
        let id = layout
            .session_id()
            .expect("a session directory is named by its id");
        assert!(is_id(id), "a session directory is named by its id: {id:?}");
        if self.store.rev_parse(&object_ref(id))?.is_some() {
            return Err(StoreError::ObjectExists(id.to_string()));
        }
        let read = |path: &Path| fs::read(path).map_err(StoreError::ReadContent);
        let attrs_value: Value = serde_json::from_slice(&read(&layout.attrs_file())?)
            .map_err(|e| StoreError::Parse(format!("session attrs {id}: {e}")))?;
        let attrs: SessionAttrsRecord = serde_json::from_value(attrs_value.clone())
            .map_err(|e| StoreError::Parse(format!("session attrs {id}: {e}")))?;
        let changes = read(&layout.changes_file())?;
        parse_changes(&changes)?;
        let files_tree =
            import_opaque_tree(self.store.path(), &layout.files_dir())?.ok_or_else(|| {
                StoreError::Parse(format!(
                    "session {id}: no {} directory",
                    layout.files_dir().display()
                ))
            })?;

        let mut tree = ObjectTree {
            attrs: Some(attrs_value),
            ..ObjectTree::default()
        };
        tree.blobs.insert(CHANGES_FILE.to_string(), changes);
        tree.subtrees.insert(FILES_TREE.to_string(), files_tree);
        let message = format!("session: {}", attrs.origin);
        self.store
            .create(OBJECT_TYPE, OBJECT_VERSION, id, &tree, &message)
    }

    /// Remove stored sessions. Every id or prefix is resolved to a live
    /// session before anything is written. Each dataset and share that
    /// currently holds any of the sessions is edited once to unlink all
    /// of them, then each session's ref gets a parentless tombstone
    /// commit. The native sessions in their sources are not touched. A
    /// failure after the container edits leaves the sessions live and
    /// unlinked, which `add` with a dataset repairs.
    pub fn remove(&self, ids_or_prefixes: &[String]) -> Result<SessionRemoveOutcome, StoreError> {
        let mut sessions: Vec<SessionRecord> = Vec::with_capacity(ids_or_prefixes.len());
        for id_or_prefix in ids_or_prefixes {
            let record = self.get(id_or_prefix)?;
            if !sessions.iter().any(|r| r.id == record.id) {
                sessions.push(record);
            }
        }
        let ids: Vec<String> = sessions.iter().map(|r| r.id.clone()).collect();

        let datasets = DatasetStore::from(self.store);
        let held = datasets.containing(&ids)?;
        for members in &held {
            datasets.sessions_unlink(&members.dataset_id, &members.session_ids)?;
        }
        let shares = ShareStore::from(self.store);
        let shared = shares.containing(&ids)?;
        for members in &shared {
            shares.unlink(&members.share_id, &members.session_ids)?;
        }

        for record in &sessions {
            self.tombstone(record)?;
        }
        Ok(SessionRemoveOutcome {
            sessions,
            datasets: held,
            shares: shared,
        })
    }

    /// Write a parentless tombstone over `record`'s current commit.
    /// Datasets and shares holding the session are not edited.
    pub(crate) fn tombstone(&self, record: &SessionRecord) -> Result<(), StoreError> {
        let object = self.store.read_object(&record.commit_sha)?;
        let message = format!(
            "session remove: {}:{}",
            record.driver_name,
            record.attrs.native_id()
        );
        self.store.delete(&object, &message)?;
        Ok(())
    }

    /// Read the live session for `id_or_prefix`.
    pub fn get(&self, id_or_prefix: &str) -> Result<SessionRecord, StoreError> {
        decode(self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?)
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

    /// The `changes.json` records of the session version at
    /// `commit_sha`: what its publish applied to the original. A
    /// session that is not a publish has none.
    pub fn changes(&self, commit_sha: &str) -> Result<Vec<ChangeRecord>, StoreError> {
        let object = self.store.read_object(commit_sha)?;
        match object.tree.blobs.get(CHANGES_FILE) {
            Some(bytes) => parse_changes(bytes),
            None => Ok(Vec::new()),
        }
    }

    /// The byte tree of the session version at `commit_sha`, for the
    /// driver that wrote it to read back through
    /// [`Driver::read_stored`].
    pub fn content(&self, commit_sha: &str) -> Box<dyn ContentSource> {
        Box::new(GitContentSource::new(
            self.store.path().to_path_buf(),
            commit_sha.to_string(),
        ))
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

fn decode(object: Object) -> Result<SessionRecord, StoreError> {
    require_type(&object, OBJECT_TYPE)?;
    let attrs = decode_attrs(&object)?;
    let (driver_name, driver_version) = match attrs.driver.split_once(' ') {
        Some((n, v)) => (n.to_string(), v.to_string()),
        None => (attrs.driver.clone(), String::new()),
    };
    Ok(SessionRecord {
        type_version: object.header.version.clone(),
        id: object.header.id,
        commit_sha: object.commit_sha.clone(),
        created_ms: object.header.created_ms,
        modified_ms: object.header.modified_ms,
        attrs,
        driver_name,
        driver_version,
    })
}

fn decode_attrs(object: &Object) -> Result<SessionAttrsRecord, StoreError> {
    let commit_sha = object.commit_sha.as_str();
    let attrs_value =
        object.tree.attrs.as_ref().ok_or_else(|| {
            StoreError::Parse(format!("session {commit_sha}: missing attrs.json"))
        })?;
    serde_json::from_value(attrs_value.clone())
        .map_err(|e| StoreError::Parse(format!("session attrs {commit_sha}: {e}")))
}

/// The layout of a session directory: a directory named by the
/// session's id holding the object's tree under `session/`.
///
/// ```text
/// session/attrs.json
/// session/changes.json
/// session/files.d/**
/// ```
///
/// A publish stages the derived session this way, and the query layer
/// reads it, so both take the paths from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDirLayout {
    root: PathBuf,
}

impl SessionDirLayout {
    /// The layout rooted at `root`, `<id>/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The session's id: the directory name.
    pub fn session_id(&self) -> Option<&str> {
        self.root.file_name().and_then(|n| n.to_str())
    }

    /// The session object's tree, `session/`.
    pub fn object_dir(&self) -> PathBuf {
        self.root.join(OBJECT_DIR)
    }

    pub fn attrs_file(&self) -> PathBuf {
        self.object_dir().join(ATTRS_FILE)
    }

    pub fn changes_file(&self) -> PathBuf {
        self.object_dir().join(CHANGES_FILE)
    }

    /// The opaque content subtree, `session/files.d/`.
    pub fn files_dir(&self) -> PathBuf {
        self.object_dir().join(FILES_TREE)
    }

    /// The records of the directory's `changes.json`.
    pub fn read_changes(&self) -> Result<Vec<ChangeRecord>, StoreError> {
        let bytes = fs::read(self.changes_file()).map_err(StoreError::ReadContent)?;
        parse_changes(&bytes)
    }
}

/// A session read from a session directory rather than a commit. It
/// has no version: the directory is mutable and nothing names a
/// content state. A writer that changes the session rewrites
/// `attrs.json`, whose mtime is what a reader keys its derived state
/// on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSession {
    pub id: String,
    /// The session directory, `<id>/`.
    pub dir: PathBuf,
}

impl ExternalSession {
    pub fn layout(&self) -> SessionDirLayout {
        SessionDirLayout::new(&self.dir)
    }

    /// The session's `attrs.json` as it is now, with the file's mtime.
    pub fn read(&self) -> Result<ExternalSessionRecord, StoreError> {
        let path = self.layout().attrs_file();
        let bytes = fs::read(&path).map_err(StoreError::ReadContent)?;
        let attrs: SessionAttrsRecord = serde_json::from_slice(&bytes)
            .map_err(|e| StoreError::Parse(format!("session attrs {}: {e}", path.display())))?;
        Ok(ExternalSessionRecord {
            id: self.id.clone(),
            dir: self.dir.clone(),
            attrs,
            attrs_modified_ms: mtime_ms(&path),
        })
    }
}

/// One read of an [`ExternalSession`].
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalSessionRecord {
    pub id: String,
    pub dir: PathBuf,
    pub attrs: SessionAttrsRecord,
    /// UNIX time millis of the `attrs.json` mtime at this read.
    pub attrs_modified_ms: i64,
}

/// The mtime of `path` in UNIX millis; 0 when it cannot be read.
fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One change of a session's `changes.json`: what its publish applied
/// to the original. A line number is the original's, except on a
/// `Title` record, whose line exists only in the derived session; the
/// derived session's lines are renumbered around the dropped ones.
/// Span offsets are positions in the published text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRecord {
    pub kind: ChangeKind,
    /// The original line, 1-based: the first dropped line of a `Line`
    /// record, the line of a `Text` record. A `Title` record's line is
    /// the derived session's line of the added entry. A `Value` record
    /// is on no line and carries 0.
    pub line: u32,
    /// The last dropped line of a `Line` record, inclusive.
    pub line_end: Option<u32>,
    /// What the record refers to: nothing for a `Line`, the JSONPath
    /// selector of a `Value`, the JSON Pointer to the text value of a
    /// `Text`.
    pub reference: Option<String>,
    /// Zero-based UTF-8 byte offsets of a `Text` record's span in the
    /// published value, `span_end` exclusive. A removal is a span of
    /// zero length.
    pub span_start: Option<u64>,
    pub span_end: Option<u64>,
    /// The reason code; a `Title` record has none.
    pub reason: Option<String>,
    pub author: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// One or more entries were left out of the derived session.
    /// Stored as `"line"`.
    Entry,
    /// Every value a JSONPath selects was left out of its entry.
    Value,
    /// A byte span of one text value was removed or replaced by the
    /// marker.
    Text,
    /// An entry that sets the session's title was added at the end.
    Title,
}

impl ChangeKind {
    /// The tag that opens a record of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Entry => "line",
            ChangeKind::Value => "value",
            ChangeKind::Text => "text",
            ChangeKind::Title => "title",
        }
    }

    /// The kind a record's tag names; `None` for a tag this build does
    /// not know.
    fn from_tag(tag: &str) -> Option<ChangeKind> {
        match tag {
            "line" => Some(ChangeKind::Entry),
            "value" => Some(ChangeKind::Value),
            "text" => Some(ChangeKind::Text),
            "title" => Some(ChangeKind::Title),
            _ => None,
        }
    }
}

/// Decode the `changes.json` text: a list of records, each a list
/// whose first element names its kind:
/// `["line", line or [line, line_end], reason, author]`,
/// `["value", path, reason, author]`,
/// `["text", line, pointer, span_start, span_end, reason, author]`,
/// and `["title", line, author]`. A
/// record whose tag this build does not know is passed over. Records
/// come back in file order.
pub fn parse_changes(bytes: &[u8]) -> Result<Vec<ChangeRecord>, StoreError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| StoreError::Parse(format!("{CHANGES_FILE}: {e}")))?;
    let Some(records) = value.as_array() else {
        return Err(StoreError::Parse(format!(
            "{CHANGES_FILE}: expected a list of records"
        )));
    };
    let mut out = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let at = |e: String| StoreError::Parse(format!("{CHANGES_FILE} record {i}: {e}"));
        let fields = record
            .as_array()
            .ok_or_else(|| at("expected a list".to_string()))?;
        let tag = fields
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| at("field 0 is not a kind tag".to_string()))?;
        let Some(kind) = ChangeKind::from_tag(tag) else {
            continue;
        };
        out.push(parse_change(kind, fields).map_err(at)?);
    }
    Ok(out)
}

/// Decode the fields of one record of `kind`, tag included.
fn parse_change(kind: ChangeKind, fields: &[Value]) -> Result<ChangeRecord, String> {
    let field = |i: usize| fields.get(i).ok_or_else(|| format!("missing field {i}"));
    let text = |i: usize| {
        field(i)?
            .as_str()
            .map(String::from)
            .ok_or_else(|| format!("field {i} is not a string"))
    };
    let line_of = |v: &Value, what: &str| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("{what} is not a line number"))
    };
    let offset = |i: usize| {
        field(i)?
            .as_u64()
            .ok_or_else(|| format!("field {i} is not a byte offset"))
    };
    let expect = |n: usize| {
        if fields.len() == n {
            Ok(())
        } else {
            Err(format!("expected {n} fields, found {}", fields.len()))
        }
    };
    let record = match kind {
        ChangeKind::Entry => {
            expect(4)?;
            let (line, line_end) = match field(1)? {
                Value::Array(pair) => match pair.as_slice() {
                    [start, end] => {
                        let line = line_of(start, "range start")?;
                        let line_end = line_of(end, "range end")?;
                        if line_end < line {
                            return Err("range end is before its start".to_string());
                        }
                        (line, line_end)
                    }
                    _ => return Err("field 1 is not a line or a range".to_string()),
                },
                single => {
                    let line = line_of(single, "field 1")?;
                    (line, line)
                }
            };
            ChangeRecord {
                kind,
                line,
                line_end: Some(line_end),
                reference: None,
                span_start: None,
                span_end: None,
                reason: Some(text(2)?),
                author: text(3)?,
            }
        }
        ChangeKind::Value => {
            expect(4)?;
            ChangeRecord {
                kind,
                line: 0,
                line_end: None,
                reference: Some(text(1)?),
                span_start: None,
                span_end: None,
                reason: Some(text(2)?),
                author: text(3)?,
            }
        }
        ChangeKind::Title => {
            expect(3)?;
            ChangeRecord {
                kind,
                line: line_of(field(1)?, "field 1")?,
                line_end: None,
                reference: None,
                span_start: None,
                span_end: None,
                reason: None,
                author: text(2)?,
            }
        }
        ChangeKind::Text => {
            expect(7)?;
            let (span_start, span_end) = (offset(3)?, offset(4)?);
            if span_end < span_start {
                return Err("span end is before its start".to_string());
            }
            ChangeRecord {
                kind,
                line: line_of(field(1)?, "field 1")?,
                line_end: None,
                reference: Some(text(2)?),
                span_start: Some(span_start),
                span_end: Some(span_end),
                reason: Some(text(5)?),
                author: text(6)?,
            }
        }
    };
    Ok(record)
}

/// Encode `records` as the `changes.json` text: one record per line in
/// the order the records are given.
pub fn format_changes(records: &[ChangeRecord]) -> String {
    let mut out = String::from("[");
    for (i, record) in records.iter().enumerate() {
        out.push_str("\n  ");
        out.push_str(&change_value(record).to_string());
        if i + 1 < records.len() {
            out.push(',');
        } else {
            out.push('\n');
        }
    }
    out.push_str("]\n");
    out
}

/// One record as the list holds it, opened by its kind's tag.
fn change_value(r: &ChangeRecord) -> Value {
    let tag = r.kind.as_str();
    match r.kind {
        ChangeKind::Entry => {
            let end = r.line_end.unwrap_or(r.line);
            let lines = if end == r.line {
                json!(r.line)
            } else {
                json!([r.line, end])
            };
            json!([tag, lines, r.reason, r.author])
        }
        ChangeKind::Value => json!([tag, r.reference, r.reason, r.author]),
        ChangeKind::Title => json!([tag, r.line, r.author]),
        ChangeKind::Text => json!([
            tag,
            r.line,
            r.reference,
            r.span_start,
            r.span_end,
            r.reason,
            r.author
        ]),
    }
}

/// A [`ContentSink`] that holds each created file in memory with the
/// SHA of the blob it would be. Nothing reaches the store: the caller
/// compares the SHAs with what the store holds and writes the files
/// it decides to keep.
pub(crate) struct GitContentSink {
    files: Vec<SinkFile>,
    write_error: Option<StoreError>,
    /// The cap on total bytes across every file written for one
    /// session; `None` is unlimited.
    max_bytes: Option<u64>,
    /// Bytes accumulated across every writer opened so far.
    total_bytes: u64,
}

/// One file a driver wrote into a [`GitContentSink`].
pub(crate) struct SinkFile {
    /// The file's path relative to the content root
    path: String,
    /// The SHA of `bytes` as a blob
    sha: String,
    bytes: Vec<u8>,
}

impl GitContentSink {
    pub(crate) fn new(max_bytes: Option<u64>) -> Self {
        Self {
            files: Vec::new(),
            write_error: None,
            max_bytes,
            total_bytes: 0,
        }
    }

    pub(crate) fn into_files(self) -> Result<Vec<SinkFile>, StoreError> {
        if let Some(e) = self.write_error {
            return Err(e);
        }
        Ok(self.files)
    }

    /// Take a recorded write error, leaving none. The caller uses this
    /// to prefer a size-cap failure over the driver's own error when
    /// `write_native` aborts.
    pub(crate) fn take_write_error(&mut self) -> Option<StoreError> {
        self.write_error.take()
    }
}

impl ContentSink for GitContentSink {
    fn create<'a>(&'a mut self, path: &str) -> io::Result<Box<dyn Write + 'a>> {
        Ok(Box::new(GitBlobWriter {
            sink: self,
            path: path.to_string(),
            buffer: Vec::new(),
        }))
    }
}

struct GitBlobWriter<'a> {
    sink: &'a mut GitContentSink,
    path: String,
    buffer: Vec<u8>,
}

impl Write for GitBlobWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Enforce the cap as bytes arrive, so a runaway session stops at
        // the limit rather than buffering to exhaustion. On breach the
        // sink records `SessionTooLarge` and the write errors, which
        // aborts the copy before the file is recorded.
        if let Some(max) = self.sink.max_bytes {
            let projected = self.sink.total_bytes + buf.len() as u64;
            if projected > max {
                self.sink.write_error = Some(StoreError::SessionTooLarge {
                    size: projected,
                    max,
                });
                return Err(io::Error::other("session exceeds max storage size"));
            }
        }
        self.sink.total_bytes += buf.len() as u64;
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for GitBlobWriter<'_> {
    fn drop(&mut self) {
        if self.sink.write_error.is_some() {
            return;
        }
        let bytes = std::mem::take(&mut self.buffer);
        self.sink.files.push(SinkFile {
            path: std::mem::take(&mut self.path),
            sha: blob_sha(&bytes),
            bytes,
        });
    }
}

/// The SHA of the `files.d/` tree [`build_files_tree`] writes from
/// `(relative_path, blob_sha)` pairs. Nothing is written.
fn files_tree_sha(entries: Vec<(String, String)>) -> Result<String, StoreError> {
    validate_session_paths(&entries)?;
    files_tree(entries, &tree_sha)
}

/// Build the `files.d/` tree from `(relative_path, blob_sha)` pairs.
/// Validates the full path set once, then recurses on subdirectories.
pub(crate) fn build_files_tree(
    path: &Path,
    entries: Vec<(String, String)>,
) -> Result<String, StoreError> {
    validate_session_paths(&entries)?;
    build_files_tree_inner(path, entries)
}

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

fn directory_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(i, _)| &path[..i])
}

pub(crate) fn build_files_tree_inner(
    path: &Path,
    entries: Vec<(String, String)>,
) -> Result<String, StoreError> {
    files_tree(entries, &|entries| mktree(path, entries))
}

/// The SHA of the tree of `entries`, with each directory level made
/// into a tree by `tree`: [`mktree`] to write the trees, [`tree_sha`]
/// to name them only.
fn files_tree(
    entries: Vec<(String, String)>,
    tree: &dyn Fn(&[TreeInput<'_>]) -> Result<String, StoreError>,
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
        subtree_shas.push((dir, files_tree(sub, tree)?));
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
    tree(&entries)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::DatasetStore;
    use crate::git::{git_in, run};
    use crate::test_support::open_store;
    use gage_core::uuid::new_uuid;
    use gage_session::SessionAttrs;

    fn attrs_with_origin(origin: &str) -> SessionAttrsRecord {
        SessionAttrsRecord {
            driver: "fake 0.1".into(),
            origin: origin.into(),
            native_mtime: 0,
            native_size: 0,
            session_type: "fake".into(),
            content_format: "fake-lines 1".into(),
            size: 0,
            project: None,
            summary: SummaryAttrs::default(),
            publish: None,
        }
    }

    #[test]
    fn native_id_is_the_identifying_part_of_origin() {
        // A path-shaped source URL: the id is the last segment.
        assert_eq!(
            attrs_with_origin("claude:/home/u/.claude/projects/-w-proj/abc-123").native_id(),
            "abc-123"
        );
        // A scan task source: the id is still the last segment.
        assert_eq!(
            attrs_with_origin("session+task:SCANA/hello:greet/native-1").native_id(),
            "native-1"
        );
        // A scheme-and-id source with no path: the id is the body.
        assert_eq!(attrs_with_origin("fake:s1").native_id(), "s1");
    }
    use gage_session::{
        ContentSource, Driver as DriverTrait, DriverError, Source, SourceOptions, StoredSession,
        WrittenContent,
    };
    use serde_json::{Map, Value};
    use std::any::Any;
    use std::io::Cursor;
    use std::slice;
    use std::time::{Duration, SystemTime};

    /// A native session with fixed content, for exercising the writer.
    pub(crate) struct FakeSession {
        id: String,
        source: String,
        files: Vec<(String, Vec<u8>)>,
        pub(crate) summary: FakeAttrs,
    }

    #[derive(Clone)]
    pub(crate) struct FakeAttrs {
        size: u64,
        pub(crate) mtime: SystemTime,
        pub(crate) line_count: Option<u64>,
        pub(crate) title: Option<String>,
        /// `(input, output, cached)` when the fake reports usage
        pub(crate) tokens: Option<(u64, u64, u64)>,
        pub(crate) cost_usd: Option<f64>,
        /// What the fake derived beyond the normalized view
        pub(crate) extra: Map<String, Value>,
    }

    impl SessionAttrs for FakeAttrs {
        fn native_mtime(&self) -> SystemTime {
            self.mtime
        }
        fn native_size(&self) -> u64 {
            self.size
        }
        fn is_empty(&self) -> Option<bool> {
            Some(false)
        }
        fn project_name(&self) -> Option<&str> {
            Some("proj")
        }
        fn title(&self) -> Option<&str> {
            self.title.as_deref()
        }
        fn model(&self) -> Option<&str> {
            None
        }
        fn message_count(&self) -> Option<u64> {
            None
        }
        fn line_count(&self) -> Option<u64> {
            self.line_count
        }
        fn input_tokens(&self) -> Option<u64> {
            self.tokens.map(|t| t.0)
        }
        fn output_tokens(&self) -> Option<u64> {
            self.tokens.map(|t| t.1)
        }
        fn cached_tokens(&self) -> Option<u64> {
            self.tokens.map(|t| t.2)
        }
        fn cost_usd(&self) -> Option<f64> {
            self.cost_usd
        }
        fn extra_attrs(&self) -> Map<String, Value> {
            self.extra.clone()
        }
    }

    impl NativeSession for FakeSession {
        fn id(&self) -> &str {
            &self.id
        }

        fn session_type(&self) -> &str {
            "fake"
        }

        fn source(&self) -> &str {
            &self.source
        }

        fn attrs(&self) -> &dyn SessionAttrs {
            &self.summary
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// A driver whose `write_native` iterates the fake session's
    /// in-memory files into the sink, and whose ordering is by line
    /// count, as a driver of an append-only transcript orders.
    pub(crate) struct FakeDriver;

    /// Write `fake`'s files into `sink` and report its attrs.
    fn write_fake(
        fake: &FakeSession,
        sink: &mut dyn ContentSink,
    ) -> Result<WrittenContent, DriverError> {
        for (path, bytes) in &fake.files {
            let mut w = sink.create(path).map_err(DriverError::Io)?;
            let mut cursor = Cursor::new(bytes.clone());
            std::io::copy(&mut cursor, &mut w).map_err(DriverError::Io)?;
        }
        Ok(WrittenContent::new(
            "fake-lines 1".to_string(),
            Box::new(fake.summary.clone()),
        ))
    }

    impl DriverTrait for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn version(&self) -> &'static str {
            "0.1"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["fake"]
        }
        fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("open_source not used".into()))
        }
        fn split_session_url(&self, _session_url: &str) -> Result<(String, String), DriverError> {
            Err(DriverError::Other("split_session_url not used".into()))
        }
        fn write_native(
            &self,
            session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            let fake = session
                .as_any()
                .downcast_ref::<FakeSession>()
                .ok_or_else(|| DriverError::Other("not a FakeSession".into()))?;
            write_fake(fake, sink)
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("read_stored not used in tests".into()))
        }
        fn native_order(&self, native: &SummaryAttrs, stored: &SummaryAttrs) -> Ordering {
            match (native.line_count, stored.line_count) {
                (Some(native), Some(stored)) => native.cmp(&stored),
                _ => Ordering::Greater,
            }
        }
    }

    /// `FakeDriver` at another version.
    struct VersionedDriver(&'static str);

    impl DriverTrait for VersionedDriver {
        fn name(&self) -> &'static str {
            FakeDriver.name()
        }
        fn version(&self) -> &'static str {
            self.0
        }
        fn schemes(&self) -> &'static [&'static str] {
            FakeDriver.schemes()
        }
        fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            FakeDriver.open_source(options)
        }
        fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
            FakeDriver.split_session_url(session_url)
        }
        fn write_native(
            &self,
            session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            FakeDriver.write_native(session, sink)
        }
        fn read_stored(
            &self,
            native_id: String,
            content_format: &str,
            source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            FakeDriver.read_stored(native_id, content_format, source)
        }
    }

    pub(crate) fn fake(id: &str, files: &[(&str, &str)]) -> FakeSession {
        let entries: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect();
        let size = entries.iter().map(|(_, b)| b.len() as u64).sum();
        FakeSession {
            id: id.to_string(),
            source: format!("fake:{id}"),
            files: entries,
            summary: FakeAttrs {
                size,
                mtime: SystemTime::UNIX_EPOCH,
                line_count: None,
                title: None,
                tokens: None,
                cost_usd: None,
                extra: Map::new(),
            },
        }
    }

    /// `fake` with the line count a driver would derive from its
    /// content
    fn fake_lines(id: &str, content: &str) -> FakeSession {
        let mut session = fake(id, &[("session.jsonl", content)]);
        session.summary.line_count = Some(content.lines().count() as u64);
        session
    }

    fn files_tree_sha(store: &Store, commit: &str) -> String {
        store.read_object(commit).unwrap().tree.subtrees[FILES_TREE].clone()
    }

    /// A session whose driver reports usage, written and read back:
    /// the totals and cost land in `attrs.summary` and are absent from
    /// the attrs of a session whose driver reports none.
    #[test]
    fn add_records_token_totals_and_cost_when_the_driver_reports_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let mut with = fake("s1", &[("session.jsonl", "{}\n")]);
        with.summary.tokens = Some((1200, 300, 1000));
        with.summary.cost_usd = Some(0.0421);
        let outcome = sessions.add(&FakeDriver, &mut with).unwrap();
        let record = sessions.at_commit(&outcome.commit_sha).unwrap();
        assert_eq!(record.attrs.summary.input_tokens, Some(1200));
        assert_eq!(record.attrs.summary.output_tokens, Some(300));
        assert_eq!(record.attrs.summary.cached_tokens, Some(1000));
        assert_eq!(record.attrs.summary.cost_usd, Some(0.0421));
        let ref_path = format!("refs/gage/1/object/{}", outcome.id);
        let attrs = cat(&store, &format!("{ref_path}:attrs.json"));
        assert!(attrs.contains("\"input_tokens\":1200"), "{attrs}");

        let outcome = sessions
            .add(&FakeDriver, &mut fake("s2", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let record = sessions.at_commit(&outcome.commit_sha).unwrap();
        assert_eq!(record.attrs.summary.input_tokens, None);
        assert_eq!(record.attrs.summary.cost_usd, None);
    }

    /// What a driver derives beyond the normalized view is written to
    /// `attrs.summary` beside the normalized fields and read back into
    /// `extra` without loss.
    #[test]
    fn add_keeps_the_drivers_extra_summary_members() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let mut with = fake("s1", &[("session.jsonl", "{}\n")]);
        with.summary
            .extra
            .insert("fake_note".to_string(), Value::from("kept"));
        let outcome = sessions.add(&FakeDriver, &mut with).unwrap();
        let record = sessions.at_commit(&outcome.commit_sha).unwrap();
        assert_eq!(record.attrs.summary.extra["fake_note"], "kept");
        assert_eq!(record.attrs.summary.is_empty, Some(false));
        let ref_path = format!("refs/gage/1/object/{}", outcome.id);
        let attrs = cat(&store, &format!("{ref_path}:attrs.json"));
        assert!(
            attrs.contains("\"summary\":{\"fake_note\":\"kept\",\"is_empty\":false}"),
            "{attrs}"
        );
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

        let outcome = sessions.add(&FakeDriver, &mut session).unwrap();
        assert_eq!(outcome.outcome, SessionOutcome::Added);
        assert_eq!(outcome.id, session_object_id("fake", "s1"));

        let ref_path = object_ref(&outcome.id);
        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["attrs.json", "created", "files.d", "id", "modified", "type"]
        );
        assert_eq!(
            cat(&store, &format!("{ref_path}:type")),
            "gage::session 1\n"
        );
        assert_eq!(
            cat(&store, &format!("{ref_path}:attrs.json")),
            "{\"content_format\":\"fake-lines 1\",\"driver\":\"fake 0.1\",\"native_mtime\":0,\"native_size\":4,\"origin\":\"fake:s1\",\"project\":\"proj\",\"session_type\":\"fake\",\"size\":4,\"summary\":{\"is_empty\":false}}\n"
        );
        assert_eq!(cat(&store, &format!("{ref_path}:files.d/sub/a.txt")), "a");

        let record = sessions.at_commit(&outcome.commit_sha).unwrap();
        assert_eq!(record.id, outcome.id);
        assert_eq!(record.driver_name, "fake");
        assert_eq!(record.driver_version, "0.1");
        assert_eq!(record.attrs.session_type, "fake");
        assert_eq!(record.attrs.content_format, "fake-lines 1");
        assert_eq!(record.attrs.native_id(), "s1");
        assert_eq!(record.attrs.origin, "fake:s1");
        assert_eq!(record.attrs.project.as_deref(), Some("proj"));
        assert_eq!(record.attrs.native_size, 4);
        assert_eq!(record.attrs.native_mtime, 0);
        assert_eq!(record.attrs.summary.is_empty, Some(false));
    }

    #[test]
    fn add_is_idempotent_and_updates_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let again = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        assert_eq!(again.outcome, SessionOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);

        let grown = sessions
            .add(
                &FakeDriver,
                &mut fake("s1", &[("session.jsonl", "{}\n{}\n")]),
            )
            .unwrap();
        assert_eq!(grown.outcome, SessionOutcome::Updated);
        assert_eq!(grown.id, first.id);
        assert_ne!(grown.commit_sha, first.commit_sha);
        assert_eq!(
            cat(&store, &format!("{}:first-parent", grown.commit_sha)),
            format!("{}\n", first.commit_sha)
        );
    }

    /// `write_native` fails, so an add that reaches it errors out
    struct UnreadableDriver;

    impl DriverTrait for UnreadableDriver {
        fn name(&self) -> &'static str {
            FakeDriver.name()
        }
        fn version(&self) -> &'static str {
            FakeDriver.version()
        }
        fn schemes(&self) -> &'static [&'static str] {
            FakeDriver.schemes()
        }
        fn open_source(&self, _options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("open_source not used".into()))
        }
        fn split_session_url(&self, _session_url: &str) -> Result<(String, String), DriverError> {
            Err(DriverError::Other("split_session_url not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            _sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            Err(DriverError::Other("content was read".into()))
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("read_stored not used in tests".into()))
        }
    }

    /// A `write_native` that fails fails the add, and the object's
    /// ref still names the commit it had.
    #[test]
    fn a_failing_write_fails_the_add_without_a_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let err = sessions
            .add(
                &UnreadableDriver,
                &mut fake("s1", &[("session.jsonl", "{}\n{}\n")]),
            )
            .unwrap_err();
        assert!(err.to_string().contains("content was read"), "{err}");
        assert_eq!(
            store.rev_parse(&object_ref(&first.id)).unwrap().unwrap(),
            first.commit_sha
        );
    }

    /// The decision rests on the content's tree SHA alone: the same
    /// bytes under another mtime are unchanged.
    #[test]
    fn re_add_of_the_same_bytes_is_unchanged_whatever_the_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n"))
            .unwrap();
        let mut touched = fake_lines("s1", "{}\n");
        touched.summary.mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        let again = sessions.add(&FakeDriver, &mut touched).unwrap();
        assert_eq!(again.outcome, SessionOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);
    }

    /// Content the driver orders before the stored state is an older
    /// copy and leaves the store as it is; content it orders equal,
    /// a session modified without growing, is written.
    #[test]
    fn differing_content_is_written_unless_the_driver_orders_it_older() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let three = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n{}\n{}\n"))
            .unwrap();
        let older = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n"))
            .unwrap();
        assert_eq!(older.outcome, SessionOutcome::Older);
        assert_eq!(older.commit_sha, three.commit_sha);

        let modified = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n{\"a\":1}\n{}\n"))
            .unwrap();
        assert_eq!(modified.outcome, SessionOutcome::Updated);
        assert_ne!(modified.commit_sha, three.commit_sha);
        assert_eq!(
            cat(&store, &format!("{}:first-parent", modified.commit_sha)),
            format!("{}\n", three.commit_sha)
        );
    }

    /// A driver version change alone rewrites the attrs and keeps the
    /// content tree; the same version writes nothing.
    #[test]
    fn a_driver_version_change_rewrites_attrs_and_keeps_the_content() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n"))
            .unwrap();
        let bumped = sessions
            .add(&VersionedDriver("0.2"), &mut fake_lines("s1", "{}\n"))
            .unwrap();
        assert_eq!(bumped.outcome, SessionOutcome::Updated);
        assert_ne!(bumped.commit_sha, first.commit_sha);
        assert_eq!(
            files_tree_sha(&store, &bumped.commit_sha),
            files_tree_sha(&store, &first.commit_sha)
        );
        assert_eq!(sessions.get(&first.id).unwrap().attrs.driver, "fake 0.2");

        let same = sessions
            .add(&VersionedDriver("0.2"), &mut fake_lines("s1", "{}\n"))
            .unwrap();
        assert_eq!(same.outcome, SessionOutcome::Unchanged);
        assert_eq!(same.commit_sha, bumped.commit_sha);
    }

    /// The `(loose, packed)` object counts of the store's object
    /// database.
    fn object_counts(store: &Store) -> (u64, u64) {
        let out = run(git_in(store.path(), ["count-objects", "-v"])).unwrap();
        let field = |name: &str| {
            out.lines()
                .find_map(|line| line.strip_prefix(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        (field("count:"), field("in-pack:"))
    }

    /// A pack leaves no loose file to find, and an unchanged session
    /// is settled from SHAs alone: the re-add writes no object.
    #[test]
    fn re_add_of_an_unchanged_session_writes_nothing_to_a_packed_store() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n{}\n"))
            .unwrap();
        run(git_in(store.path(), ["gc", "--quiet"])).unwrap();
        let packed = object_counts(&store);
        assert_eq!(packed.0, 0, "gc should pack every object");

        let again = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n{}\n"))
            .unwrap();
        assert_eq!(again.outcome, SessionOutcome::Unchanged);
        assert_eq!(again.commit_sha, first.commit_sha);
        assert_eq!(object_counts(&store), packed);
    }

    /// A driver version change commits new attrs over content the
    /// live object holds: on a packed store the content is not
    /// written again as loose objects.
    #[test]
    fn a_driver_version_change_does_not_rewrite_packed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let first = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n{}\n"))
            .unwrap();
        run(git_in(store.path(), ["gc", "--quiet"])).unwrap();

        let bumped = sessions
            .add(&VersionedDriver("0.2"), &mut fake_lines("s1", "{}\n{}\n"))
            .unwrap();
        assert_eq!(bumped.outcome, SessionOutcome::Updated);
        let files_tree = files_tree_sha(&store, &first.commit_sha);
        assert_eq!(files_tree_sha(&store, &bumped.commit_sha), files_tree);
        let blob = run(git_in(
            store.path(),
            ["rev-parse", &format!("{files_tree}:session.jsonl")],
        ))
        .unwrap();
        for sha in [files_tree.as_str(), blob.trim()] {
            let (dir, file) = sha.split_at(2);
            assert!(
                !store.path().join("objects").join(dir).join(file).exists(),
                "{sha} was written as a loose object"
            );
        }
    }

    /// Content the driver orders before the stored state is refused
    /// before any of it reaches the store.
    #[test]
    fn a_refused_older_copy_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{}\n{}\n{}\n"))
            .unwrap();
        let before = object_counts(&store);
        let older = sessions
            .add(&FakeDriver, &mut fake_lines("s1", "{\"older\":1}\n"))
            .unwrap();
        assert_eq!(older.outcome, SessionOutcome::Older);
        assert_eq!(object_counts(&store), before);
    }

    /// `FakeDriver` whose written content does not derive.
    struct UnderivableDriver;

    impl DriverTrait for UnderivableDriver {
        fn name(&self) -> &'static str {
            FakeDriver.name()
        }
        fn version(&self) -> &'static str {
            FakeDriver.version()
        }
        fn schemes(&self) -> &'static [&'static str] {
            FakeDriver.schemes()
        }
        fn open_source(&self, options: &SourceOptions) -> Result<Box<dyn Source>, DriverError> {
            FakeDriver.open_source(options)
        }
        fn split_session_url(&self, session_url: &str) -> Result<(String, String), DriverError> {
            FakeDriver.split_session_url(session_url)
        }
        fn write_native(
            &self,
            session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<WrittenContent, DriverError> {
            let written = FakeDriver.write_native(session, sink)?;
            Ok(WrittenContent::with_derive(written.content_format, || {
                Err(DriverError::Other("content does not derive".into()))
            }))
        }
        fn read_stored(
            &self,
            native_id: String,
            content_format: &str,
            source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            FakeDriver.read_stored(native_id, content_format, source)
        }
    }

    /// A session whose content does not derive fails the add before
    /// any of the content reaches the store.
    #[test]
    fn a_failed_derive_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);

        let before = object_counts(&store);
        let err = sessions
            .add(&UnderivableDriver, &mut fake_lines("s1", "{}\n"))
            .unwrap_err();
        assert!(err.to_string().contains("content does not derive"), "{err}");
        assert_eq!(object_counts(&store), before);
    }

    #[test]
    fn remove_writes_parentless_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let added = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let ref_path = object_ref(&added.id);
        let created_before = cat(&store, &format!("{ref_path}:created"));

        let outcome = sessions.remove(slice::from_ref(&added.id)).unwrap();
        assert!(outcome.datasets.is_empty());
        let removed = &outcome.sessions[0];
        assert_eq!(removed.id, added.id);
        assert_eq!(removed.commit_sha, added.commit_sha);
        assert_eq!(removed.attrs.native_id(), "s1");

        let tip = store.rev_parse(&ref_path).unwrap().unwrap();
        let commit = cat(&store, &tip);
        assert!(!commit.contains("\nparent "), "{commit}");
        assert!(commit.contains("\nsession remove: fake:s1"), "{commit}");

        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["created", "deleted", "id", "modified", "type"]
        );
        assert_eq!(cat(&store, &format!("{ref_path}:created")), created_before);
        assert_eq!(
            cat(&store, &format!("{ref_path}:deleted")),
            cat(&store, &format!("{ref_path}:modified"))
        );
    }

    #[test]
    fn remove_hides_from_iter_and_refuses_repeat() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let keep = sessions
            .add(&FakeDriver, &mut fake("keep", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let gone = sessions
            .add(&FakeDriver, &mut fake("gone", &[("session.jsonl", "{}\n")]))
            .unwrap();

        sessions.remove(slice::from_ref(&gone.id)).unwrap();

        let ids: Vec<String> = sessions.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(ids, vec![keep.id.clone()]);
        assert!(matches!(
            sessions.remove(slice::from_ref(&gone.id)).unwrap_err(),
            StoreError::ObjectDeleted(id) if id == gone.id
        ));
        // One bad argument leaves every session untouched
        assert!(matches!(
            sessions
                .remove(&[keep.id.clone(), "zzz".to_string()])
                .unwrap_err(),
            StoreError::ObjectNotFound(p) if p == "zzz"
        ));
        assert_eq!(sessions.get(&keep.id).unwrap().id, keep.id);
        assert!(matches!(
            sessions.get(&gone.id).unwrap_err(),
            StoreError::ObjectDeleted(id) if id == gone.id
        ));
    }

    #[test]
    fn add_after_remove_supersedes_tombstone_with_same_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let first = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let ref_path = object_ref(&first.id);
        let created = cat(&store, &format!("{ref_path}:created"));
        sessions.remove(slice::from_ref(&first.id)).unwrap();

        let again = sessions
            .add(
                &FakeDriver,
                &mut fake("s1", &[("session.jsonl", "{}\n{}\n")]),
            )
            .unwrap();
        assert_eq!(again.outcome, SessionOutcome::Added);
        assert_eq!(again.id, first.id);
        assert_ne!(again.commit_sha, first.commit_sha);

        let tip = store.rev_parse(&ref_path).unwrap().unwrap();
        assert_eq!(tip, again.commit_sha);
        let commit = cat(&store, &tip);
        assert!(!commit.contains("\nparent "), "{commit}");
        assert!(commit.contains("\nsession: fake:s1"), "{commit}");
        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec!["attrs.json", "created", "files.d", "id", "modified", "type"]
        );
        assert_eq!(cat(&store, &format!("{ref_path}:created")), created);
        assert_eq!(
            cat(&store, &format!("{ref_path}:files.d/session.jsonl")),
            "{}\n{}\n"
        );

        let ids: Vec<String> = sessions.iter().unwrap().map(|r| r.unwrap().id).collect();
        assert_eq!(ids, vec![first.id.clone()]);
        assert_eq!(sessions.get(&first.id).unwrap().attrs.native_id(), "s1");

        // A later changed add edits the revived session, chaining from
        // the resurrection commit
        let grown = sessions
            .add(
                &FakeDriver,
                &mut fake("s1", &[("session.jsonl", "{}\n{}\n{}\n")]),
            )
            .unwrap();
        assert_eq!(grown.outcome, SessionOutcome::Updated);
        assert_eq!(
            cat(&store, &format!("{}:first-parent", grown.commit_sha)),
            format!("{}\n", again.commit_sha)
        );
    }

    #[test]
    fn remove_unlinks_from_every_dataset_in_one_commit_each() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let datasets = DatasetStore::from(&store);
        let s1 = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "a\n")]))
            .unwrap();
        let s2 = sessions
            .add(&FakeDriver, &mut fake("s2", &[("session.jsonl", "b\n")]))
            .unwrap();
        let s3 = sessions
            .add(&FakeDriver, &mut fake("s3", &[("session.jsonl", "c\n")]))
            .unwrap();
        let a = datasets.create().unwrap();
        let b = datasets.create().unwrap();
        datasets
            .sessions_link(&a, &[s1.id.clone(), s2.id.clone(), s3.id.clone()])
            .unwrap();
        datasets.sessions_link(&b, slice::from_ref(&s2.id)).unwrap();
        let a_before = store.rev_parse(&object_ref(&a)).unwrap().unwrap();
        let b_before = store.rev_parse(&object_ref(&b)).unwrap().unwrap();

        let outcome = sessions.remove(&[s1.id.clone(), s2.id.clone()]).unwrap();
        assert_eq!(outcome.sessions.len(), 2);
        let mut held = outcome.datasets;
        held.sort_by(|x, y| x.dataset_id.cmp(&y.dataset_id));
        let mut expected = vec![
            DatasetMembers {
                dataset_id: a.clone(),
                session_ids: vec![s1.id.clone(), s2.id.clone()],
            },
            DatasetMembers {
                dataset_id: b.clone(),
                session_ids: vec![s2.id.clone()],
            },
        ];
        expected.sort_by(|x, y| x.dataset_id.cmp(&y.dataset_id));
        assert_eq!(held, expected);

        // Each dataset advanced by exactly one commit that drops the
        // members and chains from the previous commit
        for (dataset, before, remaining) in [(&a, &a_before, vec!["s3"]), (&b, &b_before, vec![])] {
            let tip = store.rev_parse(&object_ref(dataset)).unwrap().unwrap();
            assert_ne!(&tip, before);
            assert_eq!(
                cat(&store, &format!("{tip}:first-parent")),
                format!("{before}\n")
            );
            let listed: Vec<String> = datasets
                .sessions_list(dataset)
                .unwrap()
                .into_iter()
                .map(|m| m.native_id)
                .collect();
            assert_eq!(listed, remaining);
        }
        assert!(sessions.get(&s1.id).is_err());
        assert!(sessions.get(&s2.id).is_err());
        assert_eq!(sessions.get(&s3.id).unwrap().id, s3.id);

        // The removed session content stays reachable from the dataset
        let a_tip = store.rev_parse(&object_ref(&a)).unwrap().unwrap();
        let reachable = run(git_in(store.path(), ["rev-list", &a_tip])).unwrap();
        assert!(reachable.lines().any(|l| l == s1.commit_sha), "{reachable}");
    }

    #[test]
    fn get_resolves_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let added = sessions
            .add(&FakeDriver, &mut fake("s1", &[("session.jsonl", "{}\n")]))
            .unwrap();
        let record = sessions.get(&added.id[..8]).unwrap();
        assert_eq!(record.id, added.id);
        assert_eq!(record.attrs.native_id(), "s1");
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
        let outcome = sessions.add(&FakeDriver, &mut session).unwrap();
        assert_eq!(outcome.outcome, SessionOutcome::Added);
        let ref_path = object_ref(&outcome.id);
        let listing = run(git_in(
            store.path(),
            [
                "ls-tree",
                "-r",
                "--name-only",
                &format!("{ref_path}:files.d"),
            ],
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
        type Case<'a> = (&'a [(&'a str, &'a str)], &'a str, &'a str);
        let cases: &[Case<'_>] = &[
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
            let err = sessions.add(&FakeDriver, &mut session).unwrap_err();
            match err {
                StoreError::InvalidPath { path, reason } => {
                    assert_eq!(path, *expected_path, "case {i}: {files:?}");
                    assert_eq!(reason, *expected_reason, "case {i}: {files:?}");
                }
                other => panic!("case {i} {files:?}: expected InvalidPath, got {other:?}"),
            }
        }
    }

    /// A session directory named by an id, holding `attrs.json`,
    /// `changes.json`, and `files.d/` under `session/`.
    pub(crate) fn write_session_dir(
        root: &Path,
        id: &str,
        attrs: &SessionAttrsRecord,
        changes: &str,
        files: &[(&str, &str)],
    ) -> PathBuf {
        let dir = root.join(id);
        let layout = SessionDirLayout::new(&dir);
        fs::create_dir_all(layout.files_dir()).unwrap();
        let value = serde_json::to_value(attrs).unwrap();
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        fs::write(layout.attrs_file(), bytes).unwrap();
        fs::write(layout.changes_file(), changes).unwrap();
        for (path, text) in files {
            fs::write(layout.files_dir().join(path), text).unwrap();
        }
        dir
    }

    /// `create_from_dir` commits the directory's tree as it stands,
    /// with `changes.json` as a root blob beside the session's usual
    /// entries, under the id the directory is named by; the records
    /// read back through `changes`, a session added from a native
    /// source has none, and a second create of the same id is
    /// `ObjectExists`.
    #[test]
    fn create_from_dir_commits_the_session_tree_under_the_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let native = sessions
            .add(
                &FakeDriver,
                &mut fake("s1", &[("session.jsonl", "a\nb\nc\n")]),
            )
            .unwrap();
        let id = new_uuid();
        let origin = format!("session:{}@{}", native.id, native.commit_sha);
        let changes = "[\n  [\"line\",2,\"scope\",\"user:test\"]\n]\n";
        let dir = write_session_dir(
            tmp.path(),
            &id,
            &attrs_with_origin(&origin),
            changes,
            &[("session.jsonl", "a\nc\n")],
        );
        let staged_attrs = fs::read_to_string(SessionDirLayout::new(&dir).attrs_file()).unwrap();

        let commit = sessions.create_from_dir(&dir).unwrap();

        let ref_path = object_ref(&id);
        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec![
                "attrs.json",
                "changes.json",
                "created",
                "files.d",
                "id",
                "modified",
                "type"
            ]
        );
        let cat = |path: &str| {
            run(git_in(
                store.path(),
                ["cat-file", "-p", &format!("{ref_path}:{path}")],
            ))
            .unwrap()
        };
        assert_eq!(cat("type"), "gage::session 1\n");
        assert_eq!(cat("attrs.json"), staged_attrs);
        assert_eq!(cat("changes.json"), changes);
        assert_eq!(cat("files.d/session.jsonl"), "a\nc\n");

        let created = sessions.at_commit(&commit).unwrap();
        assert_eq!(created.id, id);
        assert_eq!(created.attrs.origin, origin);
        assert_eq!(sessions.iter().unwrap().count(), 2);

        let records = sessions.changes(&commit).unwrap();
        assert_eq!(records, SessionDirLayout::new(&dir).read_changes().unwrap());
        assert_eq!(records[0].kind, ChangeKind::Entry);
        assert_eq!((records[0].line, records[0].line_end), (2, Some(2)));
        assert!(sessions.changes(&native.commit_sha).unwrap().is_empty());

        let again = sessions.create_from_dir(&dir).unwrap_err();
        assert!(matches!(again, StoreError::ObjectExists(ref existing) if *existing == id));
    }

    /// The directory name is the id, so a directory named otherwise is
    /// a caller error.
    #[test]
    #[should_panic(expected = "named by its id")]
    fn create_from_dir_requires_an_id_named_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let dir = write_session_dir(
            tmp.path(),
            "not-an-id",
            &attrs_with_origin("fake:x"),
            "[]\n",
            &[],
        );
        SessionStore::from(&store).create_from_dir(&dir).unwrap();
    }

    /// A malformed `changes.json` or a missing `files.d` is refused
    /// before anything is written.
    #[test]
    fn create_from_dir_checks_the_records_and_the_content_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let sessions = SessionStore::from(&store);
        let id = new_uuid();
        let dir = write_session_dir(tmp.path(), &id, &attrs_with_origin("fake:x"), "{}\n", &[]);
        let bad = sessions.create_from_dir(&dir).unwrap_err();
        assert!(
            matches!(bad, StoreError::Parse(ref m) if m.contains("expected a list")),
            "{bad}"
        );
        fs::write(SessionDirLayout::new(&dir).changes_file(), "[]\n").unwrap();
        fs::remove_dir(SessionDirLayout::new(&dir).files_dir()).unwrap();
        let missing = sessions.create_from_dir(&dir).unwrap_err();
        assert!(
            matches!(missing, StoreError::Parse(ref m) if m.contains("no ") && m.contains("files.d")),
            "{missing}"
        );
        assert_eq!(sessions.iter().unwrap().count(), 0);
    }

    /// An external session reads its attrs as they are now and the
    /// mtime of `attrs.json`, which a rewrite moves.
    #[test]
    fn external_session_reads_attrs_and_their_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let id = new_uuid();
        let dir = write_session_dir(tmp.path(), &id, &attrs_with_origin("fake:x"), "[]\n", &[]);
        let external = ExternalSession {
            id: id.clone(),
            dir: dir.clone(),
        };
        let before = external.read().unwrap();
        assert_eq!(before.id, id);
        assert_eq!(before.dir, dir);
        assert_eq!(before.attrs.origin, "fake:x");

        let later = SystemTime::now() + std::time::Duration::from_secs(90);
        let mut attrs = attrs_with_origin("fake:y");
        attrs.summary.title = Some("Retitled".into());
        let mut bytes = serde_json::to_vec(&attrs).unwrap();
        bytes.push(b'\n');
        fs::write(external.layout().attrs_file(), bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(external.layout().attrs_file())
            .unwrap()
            .set_modified(later)
            .unwrap();

        let after = external.read().unwrap();
        assert_eq!(after.attrs.origin, "fake:y");
        assert_eq!(after.attrs.summary.title.as_deref(), Some("Retitled"));
        assert!(after.attrs_modified_ms > before.attrs_modified_ms);
        assert_eq!(
            after.attrs_modified_ms,
            later.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
        );

        let missing = ExternalSession {
            id: "gone".into(),
            dir: tmp.path().join("gone"),
        }
        .read()
        .unwrap_err();
        assert!(matches!(missing, StoreError::ReadContent(_)), "{missing}");
    }

    /// The record parser reads every kind with its fields in the
    /// positions the design gives them, in file order, a record with an
    /// unknown tag is passed over, and a malformed record is named by
    /// its position.
    #[test]
    fn parse_changes_reads_every_record_kind() {
        let text = concat!(
            "[\n",
            r#"  ["line", [1, 4], "scope", "session:abc"],"#,
            "\n",
            r#"  ["line", 7, "scope", "user:t"],"#,
            "\n",
            r#"  ["value", "$.cwd", "system", "filter:system"],"#,
            "\n",
            r#"  ["text", 5, "/message/content", 4, 14, "pii", "filter:pii"],"#,
            "\n",
            r#"  ["title", 9, "user:t"],"#,
            "\n",
            r#"  ["later", 1]"#,
            "\n]\n",
        );
        let records = parse_changes(text.as_bytes()).unwrap();
        let record = |kind,
                      line,
                      line_end,
                      reference: Option<&str>,
                      span,
                      reason: Option<&str>,
                      author: &str| {
            let (span_start, span_end) = match span {
                Some((start, end)) => (Some(start), Some(end)),
                None => (None, None),
            };
            ChangeRecord {
                kind,
                line,
                line_end,
                reference: reference.map(String::from),
                span_start,
                span_end,
                reason: reason.map(String::from),
                author: author.into(),
            }
        };
        assert_eq!(
            records,
            [
                record(
                    ChangeKind::Entry,
                    1,
                    Some(4),
                    None,
                    None,
                    Some("scope"),
                    "session:abc"
                ),
                record(
                    ChangeKind::Entry,
                    7,
                    Some(7),
                    None,
                    None,
                    Some("scope"),
                    "user:t"
                ),
                record(
                    ChangeKind::Value,
                    0,
                    None,
                    Some("$.cwd"),
                    None,
                    Some("system"),
                    "filter:system"
                ),
                record(
                    ChangeKind::Text,
                    5,
                    None,
                    Some("/message/content"),
                    Some((4, 14)),
                    Some("pii"),
                    "filter:pii"
                ),
                record(ChangeKind::Title, 9, None, None, None, None, "user:t"),
            ]
        );
        assert!(parse_changes(b"[]\n").unwrap().is_empty());
        assert!(
            parse_changes(format_changes(&[]).as_bytes())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_changes(format_changes(&records).as_bytes()).unwrap(),
            records
        );

        let bad =
            parse_changes(br#"[["line", 1, "scope", "a"], ["text", 2, "/p", "x"]]"#).unwrap_err();
        assert!(
            matches!(bad, StoreError::Parse(ref m) if m.contains("record 1") && m.contains("expected 7 fields")),
            "{bad}"
        );
        let reversed = parse_changes(br#"[["line", [4, 1], "scope", "a"]]"#).unwrap_err();
        assert!(
            matches!(reversed, StoreError::Parse(ref m) if m.contains("before its start")),
            "{reversed}"
        );
        let zero = parse_changes(br#"[["title", 0, "a"]]"#).unwrap_err();
        assert!(
            matches!(zero, StoreError::Parse(ref m) if m.contains("not a line number")),
            "{zero}"
        );
        let untagged = parse_changes(br#"[[1, "scope", "a"]]"#).unwrap_err();
        assert!(
            matches!(untagged, StoreError::Parse(ref m) if m.contains("record 0") && m.contains("not a kind tag")),
            "{untagged}"
        );
        let object = parse_changes(b"{}\n").unwrap_err();
        assert!(
            matches!(object, StoreError::Parse(ref m) if m.contains("expected a list")),
            "{object}"
        );
    }

    /// The writer puts one record per line in the order given, each
    /// opened by its tag, and a single dropped line is a bare number
    /// where a run is a pair.
    #[test]
    fn format_changes_writes_one_record_per_line() {
        let records = [
            ChangeRecord {
                kind: ChangeKind::Entry,
                line: 2,
                line_end: Some(2),
                reference: None,
                span_start: None,
                span_end: None,
                reason: Some("scope".into()),
                author: "user:t".into(),
            },
            ChangeRecord {
                kind: ChangeKind::Entry,
                line: 4,
                line_end: Some(6),
                reference: None,
                span_start: None,
                span_end: None,
                reason: Some("scope".into()),
                author: "user:t".into(),
            },
            ChangeRecord {
                kind: ChangeKind::Text,
                line: 1,
                line_end: None,
                reference: Some("/message/content".into()),
                span_start: Some(6),
                span_end: Some(16),
                reason: Some("pii".into()),
                author: "filter:pii".into(),
            },
            ChangeRecord {
                kind: ChangeKind::Title,
                line: 7,
                line_end: None,
                reference: None,
                span_start: None,
                span_end: None,
                reason: None,
                author: "user:t".into(),
            },
        ];
        assert_eq!(
            format_changes(&records),
            concat!(
                "[\n",
                "  [\"line\",2,\"scope\",\"user:t\"],\n",
                "  [\"line\",[4,6],\"scope\",\"user:t\"],\n",
                "  [\"text\",1,\"/message/content\",6,16,\"pii\",\"filter:pii\"],\n",
                "  [\"title\",7,\"user:t\"]\n",
                "]\n",
            )
        );
        assert_eq!(format_changes(&[]), "[]\n");
    }
}
