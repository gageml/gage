//! Issue objects: `gage::issue 1`, reached through [`IssueStore`].
//!
//! Content is `attrs.json` (name, title, author, status, and the
//! optional `status_reason` and `scan`), the description as
//! `description.txt`, `evidence.link` naming the commits of the notes
//! the issue cites, and `changes/<ulid>/` holding one entry per
//! event: `attrs.json` with the event fields and an optional
//! `message.txt`. `attrs.json` is the state; `changes/` is the
//! history. A status change or a comment is an edit commit that
//! rewrites `attrs.json` as needed and appends one change entry. An
//! issue has no target: a session it concerns is reached through the
//! notes it cites. Tree construction, commit parents, edits, and
//! tombstones are the generic object model's job; see
//! [`crate::object`].

use std::fmt;
use std::str::FromStr;

use gage_core::uuid::{new_ulid, new_uuid, ulid_timestamp_ms};
use serde::{Deserialize, Serialize};

use crate::git::{EntryKind, TreeEntry};
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::note::OBJECT_TYPE as NOTE_TYPE;
use crate::object::{EditOutcome, Object, ObjectTree};
use crate::writer::{TreeInput, mktree, write_blob};
use crate::{Store, StoreError};

pub const OBJECT_TYPE: &str = "gage::issue";
const OBJECT_VERSION: &str = "1";
/// Attribute paths the index extracts from an issue's `attrs.json`.
pub(crate) const INDEXED_ATTRS: &[&str] = &["name", "status"];
const ATTRS_FILE: &str = "attrs.json";
const DESCRIPTION_FILE: &str = "description.txt";
const EVIDENCE_LINK: &str = "evidence.link";
const CHANGES_DIR: &str = "changes";
const MESSAGE_FILE: &str = "message.txt";

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
    /// Commit SHAs from `evidence.link`, in file order
    pub evidence: Vec<String>,
    /// The change entries, oldest first
    pub changes: Vec<IssueChange>,
    pub created_ms: i64,
    pub modified_ms: i64,
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
        let evidence = self.resolve_evidence(input.evidence)?;
        let attrs = IssueAttrs {
            name: input.name.to_string(),
            title: input.title.to_string(),
            author: input.author.to_string(),
            status: input.status,
            status_reason: None,
            scan: None,
        };
        let change = ChangeAttrs {
            author: input.author.to_string(),
            event: ChangeEvent::Create,
            from_status: None,
            to_status: Some(input.status),
            reason: None,
        };
        let path = self.store.path();
        let changes_sha = changes_tree(path, &[], &change, None)?;
        let tree = build_tree(&attrs, input.description, evidence, changes_sha)?;
        let id = new_uuid();
        let message = format!("issue: {}", attrs.name);
        self.store
            .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, &message)?;
        Ok(id)
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
        let changes_sha = changes_tree(path, &existing, change, message)?;
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

/// The `changes/` tree: the `existing` entries plus one new entry for
/// `change`, named by a fresh ULID.
fn changes_tree(
    store_path: &std::path::Path,
    existing: &[TreeEntry],
    change: &ChangeAttrs,
    message: Option<&str>,
) -> Result<String, StoreError> {
    let mut json = serde_json::to_string(change)
        .map_err(|e| StoreError::Parse(format!("issue change encode: {e}")))?;
    json.push('\n');
    let attrs_sha = write_blob(store_path, json.as_bytes())?;
    let mut files = vec![TreeInput {
        mode: "100644",
        sha: &attrs_sha,
        name: ATTRS_FILE,
    }];
    let message_sha = match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(text) => Some(write_blob(store_path, text.as_bytes())?),
        None => None,
    };
    if let Some(sha) = &message_sha {
        files.push(TreeInput {
            mode: "100644",
            sha,
            name: MESSAGE_FILE,
        });
    }
    let entry_sha = mktree(store_path, &files)?;
    let ulid = new_ulid();
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
        sha: &entry_sha,
        name: &ulid,
    });
    mktree(store_path, &entries)
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
                carry_forward: None,
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
        });
        assert!(matches!(closed, Err(StoreError::IssueInput(_))));

        let missing = issues.create(IssueInput {
            name: "n",
            title: "t",
            description: None,
            author: "user:test",
            status: IssueStatus::Open,
            evidence: &["doesnotexist".to_string()],
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
