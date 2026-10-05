//! Issue objects: `gage::issue 1`, reached through [`IssueStore`].
//!
//! Content is `attrs.json` (name, title, author, status, and the
//! optional `status_reason`, `scan`, and `replace_key`), the
//! description as `description.txt`, `evidence.link` naming the
//! commits of the notes the issue cites, and `changes/<ulid>/` holding
//! one entry per event: `attrs.json` with the event fields and an
//! optional `message.txt`. `attrs.json` is the state; `changes/` is
//! the history. A status change or a comment is an edit commit that
//! rewrites `attrs.json` as needed and appends one change entry. An
//! issue has no target: a session it concerns is reached through the
//! notes it cites. Tree construction, commit parents, edits, and
//! tombstones are the generic object model's job; see
//! [`crate::object`].
//!
//! `replace_key` is the writer's own identity for the issue. A later
//! write under the same key replaces the live issue's state with a
//! new commit instead of creating a second issue: title, description,
//! evidence, and status are the new write's, and the prior state
//! stays in the commit chain and `changes/`.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use gage_core::uuid::{new_ulid, new_uuid, ulid_timestamp_ms};
use serde::{Deserialize, Serialize};

use crate::git::{EntryKind, TreeEntry};
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::note::OBJECT_TYPE as NOTE_TYPE;
use crate::object::{EditOutcome, Object, ObjectTree, object_ref};
use crate::writer::{TreeInput, mktree, write_blob};
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::issue";
const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from an issue's `attrs.json`.
pub(crate) const INDEXED_ATTRS: &[&str] = &["name", "status", "replace_key"];
const ATTRS_FILE: &str = "attrs.json";
const DESCRIPTION_FILE: &str = "description.txt";
const EVIDENCE_LINK: &str = "evidence.link";
const CHANGES_DIR: &str = "changes";
const MESSAGE_FILE: &str = "message.txt";
/// Scan directory only: the cited note ids, one per line. Apply
/// resolves them to commits and writes `evidence.link`; the ids may
/// name notes the same scan wrote, which have no commit until apply.
const EVIDENCE_FILE: &str = "evidence";
/// Scan directory only: present when the tree replaces a live issue,
/// holding the commit the writer replaced. Apply writes the tree as a
/// new commit of that issue.
const PARENT_FILE: &str = "parent";

/// Issue operations over an opened store.
pub struct IssueStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for IssueStore<'a> {
    fn from(store: &'a Store) -> Self {
        IssueStore { store }
    }
}

/// An issue's workflow state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueStatus {
    /// Written but not reviewed; the resolve workflow promotes it to
    /// open or closes it as a duplicate
    Pending,
    Open,
    Closed,
}

impl IssueStatus {
    pub const ALL: [IssueStatus; 3] =
        [IssueStatus::Pending, IssueStatus::Open, IssueStatus::Closed];

    pub fn as_str(self) -> &'static str {
        match self {
            IssueStatus::Pending => "pending",
            IssueStatus::Open => "open",
            IssueStatus::Closed => "closed",
        }
    }
}

impl fmt::Display for IssueStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IssueStatus {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, StoreError> {
        IssueStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| StoreError::IssueInput(format!("unknown issue status {s:?}")))
    }
}

/// Why an issue was closed. Meaningful only when the status is
/// `closed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusReason {
    Completed,
    WontFix,
    Duplicate,
}

impl StatusReason {
    pub const ALL: [StatusReason; 3] = [
        StatusReason::Completed,
        StatusReason::WontFix,
        StatusReason::Duplicate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StatusReason::Completed => "completed",
            StatusReason::WontFix => "wontfix",
            StatusReason::Duplicate => "duplicate",
        }
    }
}

impl fmt::Display for StatusReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StatusReason {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, StoreError> {
        StatusReason::ALL
            .into_iter()
            .find(|reason| reason.as_str() == s)
            .ok_or_else(|| StoreError::IssueInput(format!("unknown close reason {s:?}")))
    }
}

/// The kind of a change entry under `changes/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeEvent {
    /// The issue was written, with its initial status in `to_status`
    Create,
    /// The status changed
    Status,
    /// A free comment; carries a message and no status fields
    Comment,
    /// The title or description changed
    Edit,
}

impl ChangeEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeEvent::Create => "create",
            ChangeEvent::Status => "status",
            ChangeEvent::Comment => "comment",
            ChangeEvent::Edit => "edit",
        }
    }
}

/// Input to [`IssueStore::create`]. Every string is stored verbatim;
/// the caller produces `author` in the `user:` or `task:` URL form.
pub struct IssueInput<'a> {
    pub name: &'a str,
    pub title: &'a str,
    /// The description, written to `description.txt`. `None` writes
    /// no file.
    pub description: Option<&'a str>,
    pub author: &'a str,
    /// The initial status, `pending` or `open`. `closed` is rejected.
    pub status: IssueStatus,
    /// Ids, or unique prefixes, of the notes the issue cites. Each
    /// must be a live note; its current commit is linked. Repeats are
    /// linked once.
    pub evidence: &'a [String],
    /// The writer's identity for the issue, stored as `replace_key`.
    /// `None` means no later write replaces this issue.
    pub replace_key: Option<&'a str>,
}

/// One entry under `changes/`, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueChange {
    /// The entry's ULID
    pub id: String,
    /// The millisecond timestamp encoded in the ULID
    pub timestamp_ms: i64,
    pub author: String,
    pub event: ChangeEvent,
    /// Set on status events
    pub from_status: Option<IssueStatus>,
    /// Set on create and status events
    pub to_status: Option<IssueStatus>,
    /// Set when `to_status` is `closed`
    pub reason: Option<StatusReason>,
    /// The free-form body from `message.txt`
    pub message: Option<String>,
}

/// Everything a `show` view needs about one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueFull {
    pub id: String,
    pub commit_sha: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
    pub status: IssueStatus,
    pub status_reason: Option<StatusReason>,
    /// The scan the issue was written during, from `attrs.scan`
    pub scan: Option<String>,
    pub replace_key: Option<String>,
    /// Commit SHAs from `evidence.link`, in file order
    pub evidence: Vec<String>,
    /// The change entries, oldest first
    pub changes: Vec<IssueChange>,
    pub created_ms: i64,
    pub modified_ms: i64,
}

/// An issue in a scan directory, not yet created, as the running scan
/// reads its own issues. Evidence is the cited note ids, since a note
/// the same scan wrote has no commit yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDirRecord {
    pub id: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
    pub status: IssueStatus,
    /// The writing scan's id, from `attrs.scan`
    pub scan: Option<String>,
    pub replace_key: Option<String>,
    /// The cited note ids, in citation order
    pub evidence: Vec<String>,
    /// The commit this write replaces, when it replaces a live issue
    pub replaces: Option<String>,
    /// The time the issue was written, from the first change entry's
    /// ULID
    pub created_ms: i64,
}

/// The `attrs.json` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IssueAttrs {
    name: String,
    title: String,
    author: String,
    status: IssueStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status_reason: Option<StatusReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replace_key: Option<String>,
}

/// The `changes/<ulid>/attrs.json` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeAttrs {
    author: String,
    event: ChangeEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_status: Option<IssueStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_status: Option<IssueStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<StatusReason>,
}

impl IssueStore<'_> {
    /// Create an issue with a `create` change entry recording its
    /// initial status. Returns the new issue's id.
    pub fn create(&self, input: IssueInput) -> Result<String, StoreError> {
        validate_input(&input)?;
        let evidence = self.resolve_evidence(input.evidence)?;
        let attrs = create_attrs(&input, None);
        let path = self.store.path();
        let json = json_line(&create_change(&input), "issue change")?;
        let entry_sha = change_entry(path, &json, None)?;
        let changes_sha = changes_tree(path, &[], &new_ulid(), &entry_sha)?;
        let tree = build_tree(&attrs, input.description, evidence, changes_sha)?;
        let id = new_uuid();
        let message = format!("issue: {}", attrs.name);
        self.store
            .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
        Ok(id)
    }

    /// Write an issue as a tree under `dir` in a scan directory, for
    /// the scan to create at apply. `id` is the issue's id and `scan`
    /// the writing scan's id. The cited note ids are written as given
    /// and resolved at apply, so a note the same scan wrote may be
    /// cited; the caller checks that each id names a note the scan
    /// wrote or a stored note.
    pub fn write_to_dir(
        &self,
        dir: &Path,
        id: &str,
        input: &IssueInput,
        scan: &str,
    ) -> Result<(), StoreError> {
        validate_input(input)?;
        let attrs = create_attrs(input, Some(scan));
        let _ = id;
        write_issue_dir(dir, &attrs, input, &create_change(input), None)
    }

    /// Write a replacement of the live issue `prev` as a tree under
    /// `dir` in a scan directory, for the scan to apply as a new
    /// commit of that issue.
    /// The tree carries the new write's whole state; the change entry
    /// is a `status` event when the status differs from `prev`'s and
    /// an `edit` event otherwise. `dir` is named by `prev`'s id.
    pub fn write_replace_to_dir(
        &self,
        dir: &Path,
        input: &IssueInput,
        scan: &str,
        prev: &IssueFull,
    ) -> Result<(), StoreError> {
        validate_input(input)?;
        let attrs = create_attrs(input, Some(scan));
        let change = replace_change(input, prev.status);
        write_issue_dir(dir, &attrs, input, &change, Some(&prev.commit_sha))
    }

    /// Apply the issue under `dir`: create it, or when the tree
    /// replaces a live issue, write it as that issue's new commit.
    /// Returns `(id, commit SHA)`.
    pub fn apply_from_dir(&self, dir: &Path) -> Result<(String, String), StoreError> {
        let record = read_issue_dir(dir)?;
        match record.replaces {
            Some(_) => self.replace_from_dir(record),
            None => self.create_from_dir(record),
        }
    }

    /// Create the issue [`IssueStore::write_to_dir`] wrote under `dir`.
    /// The directory name is the issue's id. Each cited note id is
    /// resolved to its current commit; a note that does not exist or
    /// is deleted is an error. Idempotent: an issue whose ref already
    /// exists is not rewritten. Returns `(id, commit SHA)`.
    fn create_from_dir(&self, record: IssueDir) -> Result<(String, String), StoreError> {
        if let Some(sha) = self.store.rev_parse(&object_ref(&record.id))? {
            return Ok((record.id, sha));
        }
        let evidence = self.resolve_evidence(&record.evidence)?;
        let changes_sha = self.changes_tree(&[], &record.changes)?;
        let tree = build_tree(
            &record.attrs,
            record.description.as_deref(),
            evidence,
            changes_sha,
        )?;
        let message = format!("issue: {}", record.attrs.name);
        let sha = self
            .store
            .create(OBJECT_TYPE, OBJECT_VERSION, &record.id, &tree, &message)?;
        Ok((record.id, sha))
    }

    /// Write the tree [`IssueStore::write_replace_to_dir`] wrote as a
    /// new commit of the live issue, from its current commit, with the
    /// tree's change entries appended to its history. Idempotent: an
    /// issue whose history already holds those entries is not
    /// rewritten. Returns `(id, commit SHA)`.
    fn replace_from_dir(&self, record: IssueDir) -> Result<(String, String), StoreError> {
        let object = self.store.resolve_typed(&record.id, OBJECT_TYPE)?;
        let existing = match object.tree.subtrees.get(CHANGES_DIR) {
            Some(sha) => self.store.read_tree(sha)?,
            None => Vec::new(),
        };
        let applied = record
            .changes
            .iter()
            .all(|(ulid, _, _)| existing.iter().any(|e| &e.name == ulid));
        if applied {
            return Ok((record.id, object.commit_sha));
        }
        let evidence = self.resolve_evidence(&record.evidence)?;
        let changes_sha = self.changes_tree(&existing, &record.changes)?;
        let tree = build_tree(
            &record.attrs,
            record.description.as_deref(),
            evidence,
            changes_sha,
        )?;
        let message = format!("issue replace: {}", record.attrs.name);
        match self.store.edit(&object, &tree, &message)? {
            EditOutcome::Written(sha) => Ok((record.id, sha)),
            // A change entry is new content, so an edit always writes
            EditOutcome::Unchanged => Ok((record.id, object.commit_sha)),
        }
    }

    /// The `changes/` tree: `existing` entries plus `written`, each
    /// written as an entry tree.
    fn changes_tree(
        &self,
        existing: &[TreeEntry],
        written: &[DirChange],
    ) -> Result<String, StoreError> {
        let path = self.store.path();
        let mut entries: Vec<(String, String)> = Vec::with_capacity(written.len());
        for (ulid, change_json, message) in written {
            entries.push((
                ulid.clone(),
                change_entry(path, change_json, message.as_deref())?,
            ));
        }
        let mut inputs: Vec<TreeInput<'_>> = existing
            .iter()
            .map(|e| TreeInput {
                mode: &e.mode,
                sha: &e.sha,
                name: &e.name,
            })
            .collect();
        inputs.extend(entries.iter().map(|(ulid, sha)| TreeInput {
            mode: "040000",
            sha,
            name: ulid,
        }));
        mktree(path, &inputs)
    }

    /// Read the issue [`IssueStore::write_to_dir`] wrote under `dir`,
    /// as a running scan reads its own issues before apply.
    pub fn read_from_dir(&self, dir: &Path) -> Result<IssueDirRecord, StoreError> {
        let record = read_issue_dir(dir)?;
        let created_ms = record
            .changes
            .first()
            .and_then(|(ulid, _, _)| ulid_timestamp_ms(ulid))
            .ok_or_else(|| {
                StoreError::Parse(format!("issue dir {}: missing change entry", record.id))
            })?;
        Ok(IssueDirRecord {
            id: record.id,
            name: record.attrs.name,
            title: record.attrs.title,
            description: record.description,
            author: record.attrs.author,
            status: record.attrs.status,
            scan: record.attrs.scan,
            replace_key: record.attrs.replace_key,
            evidence: record.evidence,
            replaces: record.replaces,
            created_ms,
        })
    }

    /// The current commit of each cited note, in citation order with
    /// repeats dropped. A note that does not exist, is deleted, or is
    /// not a note is an error.
    fn resolve_evidence(&self, ids: &[String]) -> Result<Vec<String>, StoreError> {
        let mut shas: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            let note = self.store.resolve_typed(id, NOTE_TYPE)?;
            if !shas.contains(&note.commit_sha) {
                shas.push(note.commit_sha);
            }
        }
        Ok(shas)
    }

    /// Look up one issue by full id or unique prefix.
    ///
    /// Returns [`StoreError::ObjectNotFound`] when no object matches,
    /// [`StoreError::AmbiguousId`] when more than one does, and
    /// [`StoreError::WrongType`] when the match is not an issue.
    pub fn get(&self, id_or_prefix: &str) -> Result<IssueFull, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        self.decode_full(&object)
    }

    /// Read the issue at the given commit SHA.
    pub fn at_commit(&self, commit_sha: &str) -> Result<IssueFull, StoreError> {
        self.decode_full(&self.store.read_object(commit_sha)?)
    }

    /// Every live issue, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<IssueFull, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over issues.
    pub fn query(&self) -> IssueQuery<'_> {
        IssueQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// Change an issue's status, recording a `status` change entry.
    /// Closing without a reason records `completed`; a reason on any
    /// other status is an error, as is the status the issue already
    /// has. Opening clears `status_reason`. Returns the resolved id.
    pub fn set_status(
        &self,
        id_or_prefix: &str,
        status: IssueStatus,
        reason: Option<StatusReason>,
        author: &str,
        message: Option<&str>,
    ) -> Result<String, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        let mut attrs = decode_attrs(&object)?;
        if attrs.status == status {
            return Err(StoreError::IssueStatusUnchanged {
                id: object.header.id,
                status: status.as_str().to_string(),
            });
        }
        let reason = match (status, reason) {
            (IssueStatus::Closed, reason) => Some(reason.unwrap_or(StatusReason::Completed)),
            (_, None) => None,
            (other, Some(_)) => {
                return Err(StoreError::IssueInput(format!(
                    "a reason applies to closing, not to setting status {other}"
                )));
            }
        };
        let change = ChangeAttrs {
            author: author.to_string(),
            event: ChangeEvent::Status,
            from_status: Some(attrs.status),
            to_status: Some(status),
            reason,
        };
        attrs.status = status;
        attrs.status_reason = reason;
        let commit_message = match reason {
            Some(reason) => format!("issue status: {reason}"),
            None => format!("issue status: {status}"),
        };
        self.append_change(&object, attrs, &change, message, &commit_message)
    }

    /// Add a free comment to an issue as a `comment` change entry. An
    /// empty message is an error. Returns the resolved id.
    pub fn comment(
        &self,
        id_or_prefix: &str,
        author: &str,
        message: &str,
    ) -> Result<String, StoreError> {
        if message.trim().is_empty() {
            return Err(StoreError::IssueInput("comment is empty".to_string()));
        }
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        let attrs = decode_attrs(&object)?;
        let change = ChangeAttrs {
            author: author.to_string(),
            event: ChangeEvent::Comment,
            from_status: None,
            to_status: None,
            reason: None,
        };
        self.append_change(&object, attrs, &change, Some(message), "issue comment")
    }

    /// Write a child commit of `object` with `attrs` as its state and
    /// `change` appended under `changes/`. The description and the
    /// evidence link are carried as they are.
    fn append_change(
        &self,
        object: &Object,
        attrs: IssueAttrs,
        change: &ChangeAttrs,
        message: Option<&str>,
        commit_message: &str,
    ) -> Result<String, StoreError> {
        let path = self.store.path();
        let existing = match object.tree.subtrees.get(CHANGES_DIR) {
            Some(sha) => self.store.read_tree(sha)?,
            None => Vec::new(),
        };
        let json = json_line(change, "issue change")?;
        let entry_sha = change_entry(path, &json, message.map(str::as_bytes))?;
        let changes_sha = changes_tree(path, &existing, &new_ulid(), &entry_sha)?;
        let description = object
            .tree
            .blobs
            .get(DESCRIPTION_FILE)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
        let evidence = object
            .tree
            .links
            .get(EVIDENCE_LINK)
            .cloned()
            .unwrap_or_default();
        let tree = build_tree(&attrs, description.as_deref(), evidence, changes_sha)?;
        match self.store.edit(object, &tree, commit_message)? {
            // A change entry is new content, so an edit always writes
            EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(object.header.id.clone()),
        }
    }

    /// Delete an issue by writing a parentless tombstone commit.
    /// Returns the resolved id.
    pub fn delete(&self, id_or_prefix: &str) -> Result<String, StoreError> {
        let object = self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)?;
        let attrs = decode_attrs(&object)?;
        let message = format!("issue delete: {}", attrs.name);
        self.store.delete(&object, &message)?;
        Ok(object.header.id)
    }

    fn decode_full(&self, object: &Object) -> Result<IssueFull, StoreError> {
        let attrs = decode_attrs(object)?;
        let id = &object.header.id;
        let description =
            match object.tree.blobs.get(DESCRIPTION_FILE) {
                Some(bytes) => Some(String::from_utf8(bytes.clone()).map_err(|e| {
                    StoreError::Parse(format!("issue {id} {DESCRIPTION_FILE}: {e}"))
                })?),
                None => None,
            };
        let changes = match object.tree.subtrees.get(CHANGES_DIR) {
            Some(sha) => self.read_changes(id, sha)?,
            None => Vec::new(),
        };
        Ok(IssueFull {
            id: id.clone(),
            commit_sha: object.commit_sha.clone(),
            name: attrs.name,
            title: attrs.title,
            description,
            author: attrs.author,
            status: attrs.status,
            status_reason: attrs.status_reason,
            scan: attrs.scan,
            replace_key: attrs.replace_key,
            evidence: object
                .tree
                .links
                .get(EVIDENCE_LINK)
                .cloned()
                .unwrap_or_default(),
            changes,
            created_ms: marker_ms(object, "created", object.header.created_ms)?,
            modified_ms: marker_ms(object, "modified", object.header.modified_ms)?,
        })
    }

    /// The entries under the `changes/` tree at `sha`, in tree order,
    /// which is ULID order and so chronological.
    fn read_changes(&self, id: &str, sha: &str) -> Result<Vec<IssueChange>, StoreError> {
        let mut out = Vec::new();
        for entry in self.store.read_tree(sha)? {
            if entry.kind != EntryKind::Tree {
                return Err(StoreError::Parse(format!(
                    "issue {id} {CHANGES_DIR}/{}: expected a directory",
                    entry.name
                )));
            }
            let timestamp_ms = ulid_timestamp_ms(&entry.name).ok_or_else(|| {
                StoreError::Parse(format!(
                    "issue {id} {CHANGES_DIR}/{}: not a ULID",
                    entry.name
                ))
            })?;
            let mut attrs: Option<ChangeAttrs> = None;
            let mut message: Option<String> = None;
            for file in self.store.read_tree(&entry.sha)? {
                let bytes = self.store.read_blob_bytes(&file.sha)?;
                let where_ = format!("issue {id} {CHANGES_DIR}/{}/{}", entry.name, file.name);
                match file.name.as_str() {
                    ATTRS_FILE => {
                        attrs = Some(
                            serde_json::from_slice(&bytes)
                                .map_err(|e| StoreError::Parse(format!("{where_}: {e}")))?,
                        );
                    }
                    MESSAGE_FILE => {
                        message = Some(
                            String::from_utf8(bytes)
                                .map_err(|e| StoreError::Parse(format!("{where_}: {e}")))?,
                        );
                    }
                    _ => {
                        return Err(StoreError::Parse(format!("{where_}: unexpected file")));
                    }
                }
            }
            let attrs = attrs.ok_or_else(|| {
                StoreError::Parse(format!(
                    "issue {id} {CHANGES_DIR}/{}: missing {ATTRS_FILE}",
                    entry.name
                ))
            })?;
            out.push(IssueChange {
                id: entry.name,
                timestamp_ms,
                author: attrs.author,
                event: attrs.event,
                from_status: attrs.from_status,
                to_status: attrs.to_status,
                reason: attrs.reason,
                message,
            });
        }
        Ok(out)
    }
}

/// A selection over issues: filters on the indexed attributes, an
/// order, and a limit. `iter` reads matching issues one at a time.
pub struct IssueQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> IssueQuery<'a> {
    /// Select issues whose `name` equals `name`.
    pub fn name(mut self, name: &str) -> Self {
        self.query.attrs.push(("name", name.to_string()));
        self
    }

    /// Select issues with the given status.
    pub fn status(mut self, status: IssueStatus) -> Self {
        self.query
            .attrs
            .push(("status", status.as_str().to_string()));
        self
    }

    /// Select issues whose `replace_key` equals `key`.
    pub fn replace_key(mut self, key: &str) -> Self {
        self.query.attrs.push(("replace_key", key.to_string()));
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

    /// The number of issues the selection matches, ignoring any limit.
    /// Served by the index; no object is read.
    pub fn count(&self) -> Result<usize, StoreError> {
        let unlimited = ObjectQuery {
            limit: None,
            ..self.query.clone()
        };
        Ok(self.store.select(&unlimited)?.len())
    }

    /// The matching tips, in query order, without reading any object.
    pub fn tips(self) -> Result<Vec<SelectedTip>, StoreError> {
        self.store.select(&self.query)
    }

    /// Run the selection. Matching tips are resolved by the index in
    /// one step; each issue is read from the repository as the
    /// iterator advances.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<IssueFull, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let tips = store.select(&self.query)?;
        Ok(tips
            .into_iter()
            .map(move |tip| IssueStore::from(store).at_commit(&tip.sha)))
    }
}

fn build_tree(
    attrs: &IssueAttrs,
    description: Option<&str>,
    evidence: Vec<String>,
    changes_sha: String,
) -> Result<ObjectTree, StoreError> {
    let mut tree = ObjectTree {
        attrs: Some(
            serde_json::to_value(attrs)
                .map_err(|e| StoreError::Parse(format!("issue attrs encode: {e}")))?,
        ),
        ..ObjectTree::default()
    };
    if let Some(text) = description {
        tree.blobs
            .insert(DESCRIPTION_FILE.to_string(), text.as_bytes().to_vec());
    }
    if !evidence.is_empty() {
        tree.links.insert(EVIDENCE_LINK.to_string(), evidence);
    }
    tree.subtrees.insert(CHANGES_DIR.to_string(), changes_sha);
    Ok(tree)
}

/// Reject input the store cannot record as a new issue.
fn validate_input(input: &IssueInput) -> Result<(), StoreError> {
    if input.status == IssueStatus::Closed {
        return Err(StoreError::IssueInput(
            "an issue is created pending or open, not closed".to_string(),
        ));
    }
    if input.name.trim().is_empty() {
        return Err(StoreError::IssueInput("name is empty".to_string()));
    }
    if input.title.trim().is_empty() {
        return Err(StoreError::IssueInput("title is empty".to_string()));
    }
    Ok(())
}

fn create_attrs(input: &IssueInput, scan: Option<&str>) -> IssueAttrs {
    IssueAttrs {
        name: input.name.to_string(),
        title: input.title.to_string(),
        author: input.author.to_string(),
        status: input.status,
        status_reason: None,
        scan: scan.map(String::from),
        replace_key: input.replace_key.map(String::from),
    }
}

fn create_change(input: &IssueInput) -> ChangeAttrs {
    ChangeAttrs {
        author: input.author.to_string(),
        event: ChangeEvent::Create,
        from_status: None,
        to_status: Some(input.status),
        reason: None,
    }
}

/// The change entry a replacement records: a `status` event when the
/// write's status differs from `prev_status`, an `edit` otherwise.
fn replace_change(input: &IssueInput, prev_status: IssueStatus) -> ChangeAttrs {
    if input.status == prev_status {
        return ChangeAttrs {
            author: input.author.to_string(),
            event: ChangeEvent::Edit,
            from_status: None,
            to_status: None,
            reason: None,
        };
    }
    ChangeAttrs {
        author: input.author.to_string(),
        event: ChangeEvent::Status,
        from_status: Some(prev_status),
        to_status: Some(input.status),
        reason: None,
    }
}

/// Write an issue tree under `dir` in a scan directory: the attrs, the description,
/// the cited note ids once each, one change entry, and, for a
/// replacement, the replaced commit.
fn write_issue_dir(
    dir: &Path,
    attrs: &IssueAttrs,
    input: &IssueInput,
    change: &ChangeAttrs,
    replaces: Option<&str>,
) -> Result<(), StoreError> {
    let write = |path: &Path, bytes: &[u8]| -> Result<(), StoreError> {
        fs::write(path, bytes).map_err(|e| StoreError::Write {
            path: path.to_path_buf(),
            source: e,
        })
    };
    let change_dir = dir.join(CHANGES_DIR).join(new_ulid());
    fs::create_dir_all(&change_dir).map_err(|e| StoreError::Write {
        path: change_dir.clone(),
        source: e,
    })?;
    write(&dir.join(ATTRS_FILE), &json_line(attrs, "issue attrs")?)?;
    if let Some(text) = input.description {
        write(&dir.join(DESCRIPTION_FILE), text.as_bytes())?;
    }
    let mut ids: Vec<&str> = Vec::new();
    for id in input.evidence {
        if !ids.contains(&id.as_str()) {
            ids.push(id);
        }
    }
    if !ids.is_empty() {
        let content: String = ids.iter().map(|id| format!("{id}\n")).collect();
        write(&dir.join(EVIDENCE_FILE), content.as_bytes())?;
    }
    if let Some(sha) = replaces {
        write(&dir.join(PARENT_FILE), format!("{sha}\n").as_bytes())?;
    }
    write(
        &change_dir.join(ATTRS_FILE),
        &json_line(change, "issue change")?,
    )?;
    Ok(())
}

/// One-line JSON with a trailing newline, the encoding the store uses
/// for `attrs.json`.
fn json_line<T: Serialize>(value: &T, what: &str) -> Result<Vec<u8>, StoreError> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|e| StoreError::Parse(format!("{what} encode: {e}")))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// One `changes/<ulid>/` entry tree: `attrs.json` from `change_json`
/// and `message.txt` when `message` is given and not blank.
fn change_entry(
    store_path: &Path,
    change_json: &[u8],
    message: Option<&[u8]>,
) -> Result<String, StoreError> {
    let attrs_sha = write_blob(store_path, change_json)?;
    let mut files = vec![TreeInput {
        mode: "100644",
        sha: &attrs_sha,
        name: ATTRS_FILE,
    }];
    let message = message.filter(|m| !m.iter().all(u8::is_ascii_whitespace));
    let message_sha = match message {
        Some(text) => Some(write_blob(store_path, text)?),
        None => None,
    };
    if let Some(sha) = &message_sha {
        files.push(TreeInput {
            mode: "100644",
            sha,
            name: MESSAGE_FILE,
        });
    }
    mktree(store_path, &files)
}

/// The `changes/` tree: the `existing` entries plus `entry_sha` under
/// `ulid`.
fn changes_tree(
    store_path: &Path,
    existing: &[TreeEntry],
    ulid: &str,
    entry_sha: &str,
) -> Result<String, StoreError> {
    let mut entries: Vec<TreeInput<'_>> = existing
        .iter()
        .map(|e| TreeInput {
            mode: &e.mode,
            sha: &e.sha,
            name: &e.name,
        })
        .collect();
    entries.push(TreeInput {
        mode: "040000",
        sha: entry_sha,
        name: ulid,
    });
    mktree(store_path, &entries)
}

/// A change entry read from a scan directory: `(ulid, attrs.json
/// bytes, message.txt bytes)`.
type DirChange = (String, Vec<u8>, Option<Vec<u8>>);

/// An issue's files in a scan directory, decoded.
struct IssueDir {
    id: String,
    attrs: IssueAttrs,
    description: Option<String>,
    /// Cited note ids
    evidence: Vec<String>,
    /// The replaced commit, when the tree replaces a live issue
    replaces: Option<String>,
    /// In ULID order
    changes: Vec<DirChange>,
}

/// Decode the files [`IssueStore::write_to_dir`] wrote under `dir`. The
/// directory name is the issue's id.
fn read_issue_dir(dir: &Path) -> Result<IssueDir, StoreError> {
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| StoreError::InvalidPath {
            path: dir.display().to_string(),
            reason: "an issue directory is named by its id".to_string(),
        })?;
    let attrs_bytes = read_optional(&dir.join(ATTRS_FILE))?
        .ok_or_else(|| StoreError::Parse(format!("issue dir {id}: missing {ATTRS_FILE}")))?;
    let attrs: IssueAttrs = serde_json::from_slice(&attrs_bytes)
        .map_err(|e| StoreError::Parse(format!("issue dir {id} {ATTRS_FILE}: {e}")))?;
    let description =
        match read_optional(&dir.join(DESCRIPTION_FILE))? {
            Some(bytes) => Some(String::from_utf8(bytes).map_err(|e| {
                StoreError::Parse(format!("issue dir {id} {DESCRIPTION_FILE}: {e}"))
            })?),
            None => None,
        };
    let evidence: Vec<String> = match read_optional(&dir.join(EVIDENCE_FILE))? {
        Some(bytes) => String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        None => Vec::new(),
    };
    let replaces = read_optional(&dir.join(PARENT_FILE))?
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string());
    let changes_dir = dir.join(CHANGES_DIR);
    let mut ulids: Vec<String> = match fs::read_dir(&changes_dir) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .map_err(|e| StoreError::Write {
                        path: changes_dir.clone(),
                        source: e,
                    })
            })
            .collect::<Result<_, _>>()?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(StoreError::Write {
                path: changes_dir,
                source: e,
            });
        }
    };
    ulids.sort();
    let mut changes = Vec::with_capacity(ulids.len());
    for ulid in ulids {
        let entry_dir = changes_dir.join(&ulid);
        let json = read_optional(&entry_dir.join(ATTRS_FILE))?.ok_or_else(|| {
            StoreError::Parse(format!(
                "issue dir {id} {CHANGES_DIR}/{ulid}: missing {ATTRS_FILE}"
            ))
        })?;
        let message = read_optional(&entry_dir.join(MESSAGE_FILE))?;
        changes.push((ulid, json, message));
    }
    Ok(IssueDir {
        id,
        attrs,
        description,
        evidence,
        replaces,
        changes,
    })
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Write {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

fn decode_attrs(object: &Object) -> Result<IssueAttrs, StoreError> {
    let id = &object.header.id;
    let value = object
        .tree
        .attrs
        .clone()
        .ok_or_else(|| StoreError::Parse(format!("issue {id}: missing {ATTRS_FILE}")))?;
    serde_json::from_value(value)
        .map_err(|e| StoreError::Parse(format!("issue {id} {ATTRS_FILE}: {e}")))
}

fn marker_ms(object: &Object, name: &str, value: Option<i64>) -> Result<i64, StoreError> {
    value.ok_or_else(|| StoreError::Parse(format!("issue {}: missing {name}", object.header.id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{git_in, run};
    use crate::object::object_ref;
    use crate::test_support::open_store;
    use crate::{NoteInput, NoteStore, NoteValue};

    fn cat_file(store: &Store, spec: &str) -> String {
        run(git_in(store.path(), ["cat-file", "-p", spec])).unwrap()
    }

    fn rev_parse(store: &Store, id: &str) -> String {
        store.rev_parse(&object_ref(id)).unwrap().unwrap()
    }

    fn note(store: &Store, name: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text("v".to_string()),
                author: "user:test",
                target: None,
                metadata: None,
                carry_forward_key: None,
            })
            .unwrap()
    }

    fn issue(store: &Store, status: IssueStatus, evidence: &[String]) -> String {
        IssueStore::from(store)
            .create(IssueInput {
                name: "user-issue",
                title: "Something is off",
                description: Some("## Summary\n\nDetails.\n"),
                author: "user:test",
                status,
                evidence,
                replace_key: None,
            })
            .unwrap()
    }

    #[test]
    fn create_writes_attrs_description_and_create_change() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        let id = issue(&store, IssueStatus::Open, &[]);
        let ref_path = object_ref(&id);
        let listing = run(git_in(store.path(), ["ls-tree", "--name-only", &ref_path])).unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec![
                "attrs.json",
                "changes",
                "created",
                "description.txt",
                "id",
                "modified",
                "type"
            ]
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:type")),
            "gage::issue 1\n"
        );
        assert_eq!(
            cat_file(&store, &format!("{ref_path}:attrs.json")),
            "{\"author\":\"user:test\",\"name\":\"user-issue\",\"status\":\"open\",\"title\":\"Something is off\"}\n"
        );
        let commit = cat_file(&store, &rev_parse(&store, &id));
        assert!(commit.contains("\nissue: user-issue"), "{commit}");
        assert!(!commit.contains("\nparent "), "{commit}");

        let full = IssueStore::from(&store).get(&id).unwrap();
        assert_eq!(full.status, IssueStatus::Open);
        assert_eq!(full.status_reason, None);
        assert_eq!(
            full.description.as_deref(),
            Some("## Summary\n\nDetails.\n")
        );
        assert_eq!(full.evidence, Vec::<String>::new());
        assert_eq!(full.changes.len(), 1);
        let create = &full.changes[0];
        assert_eq!(create.event, ChangeEvent::Create);
        assert_eq!(create.to_status, Some(IssueStatus::Open));
        assert_eq!(create.from_status, None);
        assert_eq!(create.message, None);
        assert_eq!(create.author, "user:test");
        assert!(create.timestamp_ms > 0);
    }

    #[test]
    fn create_links_evidence_notes_once_each_as_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let a = note(&store, "finding.code");
        let b = note(&store, "finding.general");
        let a_commit = rev_parse(&store, &a);
        let b_commit = rev_parse(&store, &b);

        let id = issue(
            &store,
            IssueStatus::Pending,
            &[a.clone(), b.clone(), a.clone()],
        );
        let link = cat_file(&store, &format!("{}:evidence.link", object_ref(&id)));
        assert_eq!(link, format!("{a_commit}\n{b_commit}\n"));
        let commit = cat_file(&store, &rev_parse(&store, &id));
        assert!(commit.contains(&format!("parent {a_commit}")), "{commit}");
        assert!(commit.contains(&format!("parent {b_commit}")), "{commit}");
        let full = IssueStore::from(&store).get(&id).unwrap();
        assert_eq!(full.evidence, vec![a_commit, b_commit]);
        assert_eq!(full.status, IssueStatus::Pending);
    }

    #[test]
    fn create_rejects_closed_status_and_bad_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let issues = IssueStore::from(&store);
        let closed = issues.create(IssueInput {
            name: "n",
            title: "t",
            description: None,
            author: "user:test",
            status: IssueStatus::Closed,
            evidence: &[],
            replace_key: None,
        });
        assert!(matches!(closed, Err(StoreError::IssueInput(_))));

        let missing = issues.create(IssueInput {
            name: "n",
            title: "t",
            description: None,
            author: "user:test",
            status: IssueStatus::Open,
            evidence: &["doesnotexist".to_string()],
            replace_key: None,
        });
        assert!(matches!(missing, Err(StoreError::ObjectNotFound(_))));

        let gone = note(&store, "gone");
        NoteStore::from(&store).delete(&gone).unwrap();
        let deleted = issues.create(IssueInput {
            name: "n",
            title: "t",
            description: None,
            author: "user:test",
            status: IssueStatus::Open,
            evidence: &[gone.clone()],
            replace_key: None,
        });
        assert!(matches!(deleted, Err(StoreError::ObjectDeleted(id)) if id == gone));
    }

    #[test]
    fn status_changes_and_comments_append_changes_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let a = note(&store, "finding.code");
        let id = issue(&store, IssueStatus::Pending, &[a]);
        let issues = IssueStore::from(&store);
        let first_commit = rev_parse(&store, &id);

        issues
            .set_status(&id, IssueStatus::Open, None, "user:alice", Some("novel"))
            .unwrap();
        issues.comment(&id, "user:bob", "Looked into it.").unwrap();
        issues
            .set_status(
                &id,
                IssueStatus::Closed,
                Some(StatusReason::WontFix),
                "user:alice",
                None,
            )
            .unwrap();

        let full = issues.get(&id).unwrap();
        assert_eq!(full.status, IssueStatus::Closed);
        assert_eq!(full.status_reason, Some(StatusReason::WontFix));
        assert_eq!(
            full.description.as_deref(),
            Some("## Summary\n\nDetails.\n")
        );
        assert_eq!(full.evidence.len(), 1, "evidence is carried through edits");
        let events: Vec<(ChangeEvent, Option<IssueStatus>, Option<IssueStatus>)> = full
            .changes
            .iter()
            .map(|c| (c.event, c.from_status, c.to_status))
            .collect();
        assert_eq!(
            events,
            vec![
                (ChangeEvent::Create, None, Some(IssueStatus::Pending)),
                (
                    ChangeEvent::Status,
                    Some(IssueStatus::Pending),
                    Some(IssueStatus::Open)
                ),
                (ChangeEvent::Comment, None, None),
                (
                    ChangeEvent::Status,
                    Some(IssueStatus::Open),
                    Some(IssueStatus::Closed)
                ),
            ]
        );
        assert_eq!(full.changes[1].message.as_deref(), Some("novel"));
        assert_eq!(full.changes[2].message.as_deref(), Some("Looked into it."));
        assert_eq!(full.changes[2].author, "user:bob");
        assert_eq!(full.changes[3].reason, Some(StatusReason::WontFix));
        assert_eq!(full.changes[3].message, None);
        assert!(
            full.changes.windows(2).all(|w| w[0].id < w[1].id),
            "change ids sort chronologically"
        );

        // Each edit chains to the previous version and re-links the
        // evidence
        let chain = store.walk_parent_chain(&full.commit_sha).unwrap();
        assert_eq!(chain.len(), 4);
        assert_eq!(chain[3], first_commit);
        let commit = cat_file(&store, &full.commit_sha);
        assert!(commit.contains("\nissue status: wontfix"), "{commit}");
        assert!(
            commit.contains(&format!("parent {}", full.evidence[0])),
            "{commit}"
        );
        assert_eq!(
            cat_file(&store, &format!("{}:attrs.json", object_ref(&id))),
            "{\"author\":\"user:test\",\"name\":\"user-issue\",\"status\":\"closed\",\"status_reason\":\"wontfix\",\"title\":\"Something is off\"}\n"
        );
    }

    #[test]
    fn close_without_reason_records_completed_and_reopen_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = issue(&store, IssueStatus::Open, &[]);
        let issues = IssueStore::from(&store);
        issues
            .set_status(&id, IssueStatus::Closed, None, "user:test", None)
            .unwrap();
        let full = issues.get(&id).unwrap();
        assert_eq!(full.status_reason, Some(StatusReason::Completed));
        assert_eq!(full.changes[1].reason, Some(StatusReason::Completed));

        issues
            .set_status(&id, IssueStatus::Open, None, "user:test", None)
            .unwrap();
        let full = issues.get(&id).unwrap();
        assert_eq!(full.status, IssueStatus::Open);
        assert_eq!(full.status_reason, None);
        assert!(
            !cat_file(&store, &format!("{}:attrs.json", object_ref(&id))).contains("status_reason")
        );
    }

    #[test]
    fn status_and_comment_input_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = issue(&store, IssueStatus::Open, &[]);
        let issues = IssueStore::from(&store);
        assert!(matches!(
            issues.set_status(&id, IssueStatus::Open, None, "user:test", None),
            Err(StoreError::IssueStatusUnchanged { id: got, status }) if got == id && status == "open"
        ));
        assert!(matches!(
            issues.set_status(
                &id,
                IssueStatus::Pending,
                Some(StatusReason::Duplicate),
                "user:test",
                None
            ),
            Err(StoreError::IssueInput(_))
        ));
        assert!(matches!(
            issues.comment(&id, "user:test", "  \n"),
            Err(StoreError::IssueInput(_))
        ));
        // Nothing above wrote a commit
        assert_eq!(issues.get(&id).unwrap().changes.len(), 1);
    }

    #[test]
    fn write_to_dir_and_create_resolve_evidence_at_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let issues = IssueStore::from(&store);
        let input = IssueInput {
            name: "findings",
            title: "Written",
            description: Some("body"),
            author: "task:s:t",
            status: IssueStatus::Pending,
            evidence: &["NOTELATER".to_string(), "NOTELATER".to_string()],
            replace_key: None,
        };
        let dir = tmp.path().join("issues").join("ISSUE1");
        issues
            .write_to_dir(&dir, "ISSUE1", &input, "SCAN1")
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("evidence")).unwrap(),
            "NOTELATER\n",
            "cited ids are written once each, unresolved"
        );
        assert!(!dir.join("evidence.link").exists());

        let record = issues.read_from_dir(&dir).unwrap();
        assert_eq!(record.id, "ISSUE1");
        assert_eq!(record.status, IssueStatus::Pending);
        assert_eq!(record.scan.as_deref(), Some("SCAN1"));
        assert_eq!(record.evidence, ["NOTELATER"]);
        assert_eq!(record.description.as_deref(), Some("body"));
        assert!(record.created_ms > 0);

        // The cited note does not exist yet: apply fails and writes
        // nothing
        assert!(matches!(
            issues.apply_from_dir(&dir),
            Err(StoreError::ObjectNotFound(_))
        ));
        assert!(store.rev_parse(&object_ref("ISSUE1")).unwrap().is_none());

        // Once the note exists, apply links its commit
        let note_id = note(&store, "finding.code");
        let note_commit = rev_parse(&store, &note_id);
        fs::write(dir.join("evidence"), format!("{note_id}\n")).unwrap();
        let (id, sha) = issues.apply_from_dir(&dir).unwrap();
        assert_eq!(id, "ISSUE1");
        let full = issues.at_commit(&sha).unwrap();
        assert_eq!(full.evidence, [note_commit.clone()]);
        assert_eq!(full.scan.as_deref(), Some("SCAN1"));
        assert_eq!(full.changes.len(), 1);
        assert_eq!(full.changes[0].event, ChangeEvent::Create);
        assert_eq!(full.changes[0].timestamp_ms, record.created_ms);
        assert!(
            store
                .read_commit(&sha)
                .unwrap()
                .parents
                .contains(&note_commit)
        );
        // Idempotent
        assert_eq!(issues.apply_from_dir(&dir).unwrap(), (id, sha));
    }

    #[test]
    fn write_replace_and_apply_write_a_new_commit_of_the_prior_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let issues = IssueStore::from(&store);
        let first_note = note(&store, "empty-thinking");
        let prior_id = issues
            .create(IssueInput {
                name: "hidden-thinking",
                title: "Thinking hidden",
                description: Some("first"),
                author: "task:s:report",
                status: IssueStatus::Open,
                evidence: &[first_note.clone()],
                replace_key: Some("hidden-thinking"),
            })
            .unwrap();
        issues
            .set_status(&prior_id, IssueStatus::Closed, None, "user:test", None)
            .unwrap();
        let prior = issues.get(&prior_id).unwrap();
        assert_eq!(prior.replace_key.as_deref(), Some("hidden-thinking"));
        assert_eq!(
            issues
                .query()
                .replace_key("hidden-thinking")
                .tips()
                .unwrap()
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            [prior_id.as_str()],
            "the key is indexed"
        );

        let second_note = note(&store, "empty-thinking");
        let input = IssueInput {
            name: "hidden-thinking",
            title: "Thinking hidden again",
            description: Some("second"),
            author: "task:s:report",
            status: IssueStatus::Open,
            evidence: &[second_note.clone()],
            replace_key: Some("hidden-thinking"),
        };
        let dir = tmp.path().join("issues").join(&prior_id);
        issues
            .write_replace_to_dir(&dir, &input, "SCAN2", &prior)
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("parent")).unwrap().trim(),
            prior.commit_sha
        );
        let record = issues.read_from_dir(&dir).unwrap();
        assert_eq!(record.id, prior_id);
        assert_eq!(record.replaces.as_deref(), Some(prior.commit_sha.as_str()));
        assert_eq!(record.replace_key.as_deref(), Some("hidden-thinking"));

        let (id, sha) = issues.apply_from_dir(&dir).unwrap();
        assert_eq!(id, prior_id);
        assert_ne!(sha, prior.commit_sha);
        let full = issues.at_commit(&sha).unwrap();
        assert_eq!(full.status, IssueStatus::Open);
        assert_eq!(full.status_reason, None);
        assert_eq!(full.title, "Thinking hidden again");
        assert_eq!(full.description.as_deref(), Some("second"));
        assert_eq!(full.scan.as_deref(), Some("SCAN2"));
        assert_eq!(
            full.evidence,
            [rev_parse(&store, &second_note)],
            "evidence is the new write's, not appended"
        );
        assert_eq!(
            full.changes
                .iter()
                .map(|c| (c.event, c.from_status, c.to_status))
                .collect::<Vec<_>>(),
            [
                (ChangeEvent::Create, None, Some(IssueStatus::Open)),
                (
                    ChangeEvent::Status,
                    Some(IssueStatus::Open),
                    Some(IssueStatus::Closed)
                ),
                (
                    ChangeEvent::Status,
                    Some(IssueStatus::Closed),
                    Some(IssueStatus::Open)
                ),
            ],
            "the prior history is kept and the replacement appends its entry"
        );
        assert_eq!(
            store.read_commit(&sha).unwrap().parents[0],
            prior.commit_sha,
            "the new commit is a version of the prior issue"
        );
        assert_eq!(issues.get(&prior_id).unwrap().commit_sha, sha);
        // Idempotent
        assert_eq!(issues.apply_from_dir(&dir).unwrap(), (id, sha));

        // A replacement with the same status records an edit
        let same = issues.get(&prior_id).unwrap();
        let dir2 = tmp.path().join("issues2").join(&prior_id);
        issues
            .write_replace_to_dir(&dir2, &input, "SCAN3", &same)
            .unwrap();
        let (_, sha3) = issues.apply_from_dir(&dir2).unwrap();
        let full = issues.at_commit(&sha3).unwrap();
        assert_eq!(full.changes.last().unwrap().event, ChangeEvent::Edit);
        assert_eq!(full.scan.as_deref(), Some("SCAN3"));
    }

    #[test]
    fn query_filters_on_name_and_status_and_delete_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let issues = IssueStore::from(&store);
        let open = issue(&store, IssueStatus::Open, &[]);
        let pending = issue(&store, IssueStatus::Pending, &[]);
        let other = issues
            .create(IssueInput {
                name: "hidden-thinking",
                title: "Thinking hidden",
                description: None,
                author: "task:hidden-thinking:report",
                status: IssueStatus::Open,
                evidence: &[],
                replace_key: None,
            })
            .unwrap();

        let ids = |q: IssueQuery| -> Vec<String> {
            let mut ids: Vec<String> = q.tips().unwrap().into_iter().map(|t| t.id).collect();
            ids.sort();
            ids
        };
        assert_eq!(issues.query().count().unwrap(), 3);
        let mut expect_open = vec![open.clone(), other.clone()];
        expect_open.sort();
        assert_eq!(ids(issues.query().status(IssueStatus::Open)), expect_open);
        assert_eq!(
            ids(issues.query().status(IssueStatus::Pending)),
            vec![pending.clone()]
        );
        assert_eq!(
            ids(issues.query().name("hidden-thinking")),
            vec![other.clone()]
        );

        issues
            .set_status(&pending, IssueStatus::Closed, None, "user:test", None)
            .unwrap();
        assert_eq!(
            ids(issues.query().status(IssueStatus::Closed)),
            vec![pending.clone()]
        );
        assert_eq!(
            ids(issues.query().status(IssueStatus::Pending)),
            Vec::<String>::new()
        );

        issues.delete(&other).unwrap();
        assert_eq!(issues.query().count().unwrap(), 2);
        assert!(matches!(
            issues.get(&other),
            Err(StoreError::ObjectDeleted(id)) if id == other
        ));
        let names: Vec<String> = issues.iter().unwrap().map(|r| r.unwrap().name).collect();
        assert_eq!(names, vec!["user-issue", "user-issue"]);
    }
}
