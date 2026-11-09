//! Share objects: `gage::share 1`, reached through [`ShareStore`].
//!
//! A share is the set of published sessions one pushes to others.
//! Content is `attrs.json`, holding a description and the filters
//! every member was published with, and `sessions.link` listing the
//! commit SHA of each member in insertion order. Every listed SHA is a
//! commit parent, so pushing a share carries its members along. A
//! session object has no link files of its own, so the share's link
//! closure is its members and nothing else.
//!
//! Membership is a rule the store applies on every write that places
//! a session: the session is published, and its publish applied every
//! filter the share names at a version the share accepts. A write that
//! would place a non-compliant session fails before anything is
//! written. Tree construction, commit parents, and edits are the
//! generic object model's job; see [`crate::object`].

use std::fmt;
use std::str::FromStr;

use gage_core::uuid::new_uuid;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::dataset::DatasetStore;
use crate::index::{ObjectQuery, Order, SelectedTip};
use crate::object::{EditOutcome, Object, ObjectTree, require_type};
use crate::session::{PublishAttrs, PublishFilter, SessionOutcome, SessionRecord, SessionStore};
use crate::{Store, StoreError, url};

pub const OBJECT_TYPE: &str = "gage::share";
pub(crate) const OBJECT_VERSION: &str = "1";
/// Shares declare no indexed attributes.
pub(crate) const INDEXED_ATTRS: &[&str] = &[];
pub(crate) const SESSIONS_LINK: &str = "sessions.link";

/// Share operations over an opened store.
pub struct ShareStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for ShareStore<'a> {
    fn from(store: &'a Store) -> Self {
        ShareStore { store }
    }
}

/// The share's `attrs.json`. Every field is optional and defaulted; a
/// share with neither has no `attrs.json` at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareAttrs {
    /// What the share is for, for the recipient.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The filters every member was published with, each entry in
    /// [`FilterSpec`] text form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,
}

impl ShareAttrs {
    /// The parsed filter entries.
    pub fn filter_specs(&self) -> Result<Vec<FilterSpec>, StoreError> {
        self.filters.iter().map(|f| f.parse()).collect()
    }
}

/// One filter a share requires: a name and an optional version
/// requirement.
///
/// The text form is the name followed by the requirement's
/// comparators separated by whitespace: `secret >=1.2 <2`. The
/// comparators are Cargo's `VersionReq` grammar with the comma
/// separator replaced, since the comma separates entries in a CLI
/// spec. A name alone accepts any version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSpec {
    pub name: String,
    pub requirement: Option<VersionReq>,
}

impl FilterSpec {
    /// True when a filter recorded at `version` satisfies this spec.
    /// A bare name accepts every version, including none; a
    /// requirement rejects a missing or unparsable version.
    pub fn accepts(&self, version: Option<&str>) -> bool {
        match &self.requirement {
            None => true,
            Some(req) => version
                .and_then(|v| Version::parse(v).ok())
                .is_some_and(|v| req.matches(&v)),
        }
    }
}

impl FromStr for FilterSpec {
    type Err = StoreError;

    fn from_str(entry: &str) -> Result<Self, StoreError> {
        let bad = |reason: &str| StoreError::FilterSpec(format!("{entry:?}: {reason}"));
        let mut tokens = entry.split_whitespace();
        let name = tokens.next().ok_or_else(|| bad("empty entry"))?;
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(bad("a filter name is letters, digits, '-', and '_'"));
        }
        let comparators: Vec<&str> = tokens.collect();
        let requirement = if comparators.is_empty() {
            None
        } else {
            Some(
                VersionReq::parse(&comparators.join(", "))
                    .map_err(|e| bad(&format!("version requirement: {e}")))?,
            )
        };
        Ok(FilterSpec {
            name: name.to_string(),
            requirement,
        })
    }
}

impl fmt::Display for FilterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(req) = &self.requirement {
            for comparator in &req.comparators {
                write!(f, " {comparator}")?;
            }
        }
        Ok(())
    }
}

/// Parse a comma-separated list of [`FilterSpec`] entries, the form
/// the CLI takes in one option value. Empty entries are skipped, so a
/// trailing comma is accepted.
pub fn parse_filter_specs(spec: &str) -> Result<Vec<FilterSpec>, StoreError> {
    spec.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::parse)
        .collect()
}

/// A share summary row for the list view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRecord {
    pub id: String,
    /// The share's current commit
    pub commit_sha: String,
    pub created_ms: i64,
    pub modified_ms: Option<i64>,
    pub attrs: ShareAttrs,
    /// Member sessions listed in `sessions.link`
    pub session_count: usize,
}

/// Outcome of linking one session to a share.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareLinkOutcome {
    /// Position in `sessions.link` after the operation.
    pub session_num: u32,
    pub id: String,
    pub outcome: SessionOutcome,
}

/// Outcome of unlinking one session from a share.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareUnlinkOutcome {
    /// Position in `sessions.link` before the operation.
    pub session_num: u32,
    pub id: String,
}

/// Outcome of editing a share's attrs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShareEditOutcome {
    /// Members the new filters refused and `force` unlinked, in
    /// member order
    pub unlinked: Vec<String>,
}

/// The current members of one share that belong to a queried set of
/// sessions, in member order.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareMembers {
    pub share_id: String,
    pub session_ids: Vec<String>,
}

/// Outcome of deleting a share together with its member sessions.
#[derive(Debug, PartialEq, Eq)]
pub struct ShareDeleted {
    pub share: String,
    /// Member sessions tombstoned, in member order
    pub removed: Vec<String>,
    /// Member sessions kept because another live share or dataset
    /// holds them
    pub kept: Vec<String>,
}

impl ShareStore<'_> {
    /// Create a share holding `session_ids`, each an id or prefix of a
    /// live session, in one commit. Every session is checked against
    /// `attrs` before anything is written. Returns the new id.
    pub fn create(&self, attrs: &ShareAttrs, session_ids: &[String]) -> Result<String, StoreError> {
        let specs = attrs.filter_specs()?;
        let sessions = SessionStore::from(self.store);
        let mut members: Vec<String> = Vec::with_capacity(session_ids.len());
        let mut member_ids: Vec<String> = Vec::with_capacity(session_ids.len());
        for id_or_prefix in session_ids {
            let record = sessions.get(id_or_prefix)?;
            check_membership(&specs, &record)?;
            place_member(&mut members, &mut member_ids, &record.id, record.commit_sha);
        }
        let mut tree = ObjectTree {
            attrs: attrs_value(attrs)?,
            ..ObjectTree::default()
        };
        if !members.is_empty() {
            tree.links.insert(SESSIONS_LINK.to_string(), members);
        }
        let id = new_uuid();
        self.store
            .create(OBJECT_TYPE, OBJECT_VERSION, &id, &tree, "share")?;
        Ok(id)
    }

    /// Every live share, newest created first, read lazily.
    pub fn iter(
        &self,
    ) -> Result<impl Iterator<Item = Result<ShareRecord, StoreError>> + '_, StoreError> {
        self.query().iter()
    }

    /// Start a selection over shares.
    pub fn query(&self) -> ShareQuery<'_> {
        ShareQuery {
            store: self.store,
            query: ObjectQuery::new(OBJECT_TYPE),
        }
    }

    /// Resolve a full id or unique prefix to the share's full id.
    pub fn resolve_id(&self, id_or_prefix: &str) -> Result<String, StoreError> {
        Ok(self.current(id_or_prefix)?.header.id)
    }

    /// Look up one share by full id or unique prefix.
    pub fn get(&self, id_or_prefix: &str) -> Result<ShareRecord, StoreError> {
        record(&self.current(id_or_prefix)?)
    }

    /// Read the share at the given commit SHA, which need not be the
    /// share's current commit.
    pub fn at_commit(&self, commit_sha: &str) -> Result<ShareRecord, StoreError> {
        let object = self.store.read_object(commit_sha)?;
        require_type(&object, OBJECT_TYPE)?;
        record(&object)
    }

    /// Replace the share's attrs. A member the new filters refuse
    /// fails the edit with [`StoreError::ShareMembership`] and nothing
    /// is written, unless `force`, which unlinks every such member in
    /// the same commit and reports them.
    pub fn edit(
        &self,
        id_or_prefix: &str,
        attrs: &ShareAttrs,
        force: bool,
    ) -> Result<ShareEditOutcome, StoreError> {
        let specs = attrs.filter_specs()?;
        let share = self.current(id_or_prefix)?;
        let sessions = SessionStore::from(self.store);
        let mut kept: Vec<String> = Vec::new();
        let mut unlinked: Vec<String> = Vec::new();
        for sha in members(&share) {
            let record = sessions.at_commit(&sha)?;
            match check_membership(&specs, &record) {
                Ok(()) => kept.push(sha),
                Err(e) if force => {
                    debug_assert!(matches!(e, StoreError::ShareMembership { .. }));
                    unlinked.push(record.id);
                }
                Err(e) => return Err(e),
            }
        }
        let mut tree = ObjectTree {
            attrs: attrs_value(attrs)?,
            links: share.tree.links.clone(),
            ..ObjectTree::default()
        };
        if kept.is_empty() {
            tree.links.remove(SESSIONS_LINK);
        } else {
            tree.links.insert(SESSIONS_LINK.to_string(), kept);
        }
        let message = if unlinked.is_empty() {
            "share edit".to_string()
        } else {
            format!("share edit: unlink {}", count(unlinked.len(), "session"))
        };
        match self.store.edit(&share, &tree, &message)? {
            EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(ShareEditOutcome { unlinked }),
        }
    }

    /// Link stored sessions as members of the share in a single
    /// commit. Each id or prefix resolves to a live session, checked
    /// against the share's filters before anything is written, whose
    /// current commit takes the member's slot: a new slot is
    /// [`SessionOutcome::Added`], an existing slot advanced to a newer
    /// commit is [`SessionOutcome::Updated`], and a slot already at
    /// the current commit is [`SessionOutcome::Unchanged`]. When no
    /// member SHA changes, no share commit is written.
    pub fn link(
        &self,
        id_or_prefix: &str,
        session_ids: &[String],
    ) -> Result<Vec<ShareLinkOutcome>, StoreError> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let share = self.current(id_or_prefix)?;
        let specs = attrs(&share)?.filter_specs()?;
        let mut members = members(&share);
        let mut member_ids: Vec<String> = Vec::with_capacity(members.len());
        for sha in &members {
            member_ids.push(self.store.read_header(sha)?.id);
        }

        let sessions = SessionStore::from(self.store);
        let mut outcomes: Vec<ShareLinkOutcome> = Vec::with_capacity(session_ids.len());
        for id_or_prefix in session_ids {
            let record = sessions.get(id_or_prefix)?;
            check_membership(&specs, &record)?;
            let outcome = match member_ids
                .iter()
                .position(|m| m == &record.id)
                .and_then(|idx| members.get(idx))
            {
                Some(sha) if *sha == record.commit_sha => SessionOutcome::Unchanged,
                Some(_) => SessionOutcome::Updated,
                None => SessionOutcome::Added,
            };
            let session_num =
                place_member(&mut members, &mut member_ids, &record.id, record.commit_sha);
            outcomes.push(ShareLinkOutcome {
                session_num,
                id: record.id,
                outcome,
            });
        }
        let changed: Vec<u32> = outcomes
            .iter()
            .filter(|o| o.outcome != SessionOutcome::Unchanged)
            .map(|o| o.session_num)
            .collect();
        let message = format!("share link {}", join_nums(&changed));
        self.write_members(&share, members, &message)?;
        Ok(outcomes)
    }

    /// Unlink sessions from the share in a single commit. Each id or
    /// prefix resolves to a session object, live or removed, that is
    /// a current member; a non-member is
    /// [`StoreError::SessionNotFound`] and nothing is written. Later
    /// members move up to fill the vacated positions. The session
    /// objects are not touched.
    pub fn unlink(
        &self,
        id_or_prefix: &str,
        session_ids: &[String],
    ) -> Result<Vec<ShareUnlinkOutcome>, StoreError> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let share = self.current(id_or_prefix)?;
        let members = members(&share);
        let mut member_ids: Vec<String> = Vec::with_capacity(members.len());
        for sha in &members {
            member_ids.push(self.store.read_header(sha)?.id);
        }

        let mut drop: Vec<usize> = Vec::with_capacity(session_ids.len());
        let mut outcomes: Vec<ShareUnlinkOutcome> = Vec::with_capacity(session_ids.len());
        for id_or_prefix in session_ids {
            let (id, _) = self.store.resolve_id(id_or_prefix)?;
            let idx = member_ids
                .iter()
                .position(|m| *m == id)
                .ok_or_else(|| StoreError::SessionNotFound(id_or_prefix.clone()))?;
            if drop.contains(&idx) {
                continue;
            }
            drop.push(idx);
            outcomes.push(ShareUnlinkOutcome {
                session_num: (idx + 1) as u32,
                id,
            });
        }

        let kept: Vec<String> = members
            .into_iter()
            .enumerate()
            .filter(|(idx, _)| !drop.contains(idx))
            .map(|(_, sha)| sha)
            .collect();
        let mut nums: Vec<u32> = outcomes.iter().map(|o| o.session_num).collect();
        nums.sort();
        let message = format!("share unlink {}", join_nums(&nums));
        self.write_members(&share, kept, &message)?;
        Ok(outcomes)
    }

    /// The member sessions of a share in member order, each read at
    /// the commit the share links.
    pub fn sessions(&self, id_or_prefix: &str) -> Result<Vec<SessionRecord>, StoreError> {
        let sessions = SessionStore::from(self.store);
        members(&self.current(id_or_prefix)?)
            .iter()
            .map(|sha| sessions.at_commit(sha))
            .collect()
    }

    /// The shares whose current members include any of `session_ids`,
    /// each with the subset it holds in member order. Shares holding
    /// none are omitted.
    pub fn containing(&self, session_ids: &[String]) -> Result<Vec<ShareMembers>, StoreError> {
        let mut out = Vec::new();
        for tip in self.store.select(&ObjectQuery::new(OBJECT_TYPE))? {
            let share = self.store.read_object(&tip.sha)?;
            let mut held = Vec::new();
            for sha in members(&share) {
                let id = self.store.read_header(&sha)?.id;
                if session_ids.contains(&id) && !held.contains(&id) {
                    held.push(id);
                }
            }
            if !held.is_empty() {
                out.push(ShareMembers {
                    share_id: share.header.id,
                    session_ids: held,
                });
            }
        }
        Ok(out)
    }

    /// Delete a share by writing a parentless tombstone commit. The
    /// member sessions stay. Returns the resolved id.
    pub fn delete(&self, id_or_prefix: &str) -> Result<String, StoreError> {
        let object = self.current(id_or_prefix)?;
        self.store.delete(&object, "share delete")?;
        Ok(object.header.id)
    }

    /// Delete a share and the member sessions no other live share or
    /// dataset holds. A member held elsewhere is kept and reported. A
    /// member removed since the share linked it is skipped. The
    /// sessions go first, so a failure among them leaves the share in
    /// place.
    pub fn delete_cascade(&self, id_or_prefix: &str) -> Result<ShareDeleted, StoreError> {
        let object = self.current(id_or_prefix)?;
        let mut member_ids: Vec<String> = Vec::new();
        for sha in members(&object) {
            member_ids.push(self.store.read_header(&sha)?.id);
        }
        let mut held_elsewhere: Vec<String> = self
            .containing(&member_ids)?
            .into_iter()
            .filter(|m| m.share_id != object.header.id)
            .flat_map(|m| m.session_ids)
            .collect();
        held_elsewhere.extend(
            DatasetStore::from(self.store)
                .containing(&member_ids)?
                .into_iter()
                .flat_map(|m| m.session_ids),
        );

        let mut deleted = ShareDeleted {
            share: object.header.id.clone(),
            removed: Vec::new(),
            kept: Vec::new(),
        };
        let sessions = SessionStore::from(self.store);
        for id in member_ids {
            if held_elsewhere.contains(&id) {
                deleted.kept.push(id);
                continue;
            }
            match sessions.get(&id) {
                Ok(record) => {
                    sessions.tombstone(&record)?;
                    deleted.removed.push(id);
                }
                // Already removed since the share linked it
                Err(StoreError::ObjectDeleted(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.store.delete(&object, "share delete")?;
        Ok(deleted)
    }

    /// Whether the share admits a session published with `publish`,
    /// before that session exists: the filter rule alone, since a
    /// publish is published by construction. `session` names the
    /// session in the error.
    pub fn accepts(
        &self,
        id_or_prefix: &str,
        session: &str,
        publish: &PublishAttrs,
    ) -> Result<(), StoreError> {
        let specs = attrs(&self.current(id_or_prefix)?)?.filter_specs()?;
        check_filters(&specs, session, &publish.filters)
    }

    fn current(&self, id_or_prefix: &str) -> Result<Object, StoreError> {
        self.store.resolve_typed(id_or_prefix, OBJECT_TYPE)
    }

    /// Write `members` as the share's `sessions.link` in one edit
    /// commit, keeping the attrs. Unchanged content writes nothing; an
    /// empty list drops the file.
    fn write_members(
        &self,
        share: &Object,
        members: Vec<String>,
        message: &str,
    ) -> Result<(), StoreError> {
        let mut tree = ObjectTree {
            attrs: share.tree.attrs.clone(),
            links: share.tree.links.clone(),
            ..ObjectTree::default()
        };
        if members.is_empty() {
            tree.links.remove(SESSIONS_LINK);
        } else {
            tree.links.insert(SESSIONS_LINK.to_string(), members);
        }
        match self.store.edit(share, &tree, message)? {
            EditOutcome::Unchanged | EditOutcome::Written(_) => Ok(()),
        }
    }
}

/// The membership rule: `session` is published, and its publish
/// applied every filter in `specs` at an accepted version.
fn check_membership(specs: &[FilterSpec], session: &SessionRecord) -> Result<(), StoreError> {
    let published = url::parse(&session.attrs.origin)
        .map(|u| u.scheme == "session")
        .unwrap_or(false);
    if !published {
        return Err(StoreError::ShareMembership {
            session: session.id.clone(),
            reason: "not a published session".to_string(),
        });
    }
    let applied = session
        .attrs
        .publish
        .as_ref()
        .map(|p| p.filters.as_slice())
        .unwrap_or_default();
    check_filters(specs, &session.id, applied)
}

/// The filter half of the membership rule: every spec names a filter
/// in `applied` at an accepted version.
fn check_filters(
    specs: &[FilterSpec],
    session: &str,
    applied: &[PublishFilter],
) -> Result<(), StoreError> {
    let refuse = |reason: String| StoreError::ShareMembership {
        session: session.to_string(),
        reason,
    };
    for spec in specs {
        let Some(filter) = applied.iter().find(|f| f.name == spec.name) else {
            return Err(refuse(format!("published without filter {}", spec.name)));
        };
        if !spec.accepts(filter.version.as_deref()) {
            return Err(refuse(format!(
                "filter {} at version {} does not satisfy {spec}",
                spec.name,
                filter.version.as_deref().unwrap_or("none")
            )));
        }
    }
    Ok(())
}

/// Put `commit_sha` in the slot of member `id`, replacing the slot
/// when the session is already a member and appending otherwise.
/// `member_ids` parallels `members`. Returns the 1-based slot.
fn place_member(
    members: &mut Vec<String>,
    member_ids: &mut Vec<String>,
    id: &str,
    commit_sha: String,
) -> u32 {
    match member_ids.iter().position(|m| m == id) {
        Some(idx) => {
            *members
                .get_mut(idx)
                .expect("idx came from member_ids, which parallels members") = commit_sha;
            (idx + 1) as u32
        }
        None => {
            members.push(commit_sha);
            member_ids.push(id.to_string());
            members.len() as u32
        }
    }
}

/// A selection over shares: an order and a limit. `iter` reads
/// matching shares one at a time.
pub struct ShareQuery<'a> {
    store: &'a Store,
    query: ObjectQuery,
}

impl<'a> ShareQuery<'a> {
    pub fn order(mut self, order: Order) -> Self {
        self.query.order = order;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.query.limit = Some(limit);
        self
    }

    /// The number of shares the selection matches, ignoring any
    /// limit. Served by the index; no object is read.
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

    /// Run the selection.
    pub fn iter(
        self,
    ) -> Result<impl Iterator<Item = Result<ShareRecord, StoreError>> + 'a, StoreError> {
        let store = self.store;
        let tips = store.select(&self.query)?;
        Ok(tips
            .into_iter()
            .map(move |tip| record(&store.read_object(&tip.sha)?)))
    }
}

fn record(object: &Object) -> Result<ShareRecord, StoreError> {
    let created_ms = object
        .header
        .created_ms
        .ok_or_else(|| StoreError::Parse(format!("share {}: missing created", object.header.id)))?;
    Ok(ShareRecord {
        id: object.header.id.clone(),
        commit_sha: object.commit_sha.clone(),
        created_ms,
        modified_ms: object.header.modified_ms,
        attrs: attrs(object)?,
        session_count: members(object).len(),
    })
}

fn attrs(object: &Object) -> Result<ShareAttrs, StoreError> {
    let Some(value) = &object.tree.attrs else {
        return Ok(ShareAttrs::default());
    };
    serde_json::from_value(value.clone())
        .map_err(|e| StoreError::Parse(format!("share attrs {}: {e}", object.commit_sha)))
}

/// `attrs` as the tree's `attrs.json` value: `None` when every field
/// is at its default, so such a share has no `attrs.json`. Each filter
/// entry is validated and written in its canonical text form.
fn attrs_value(attrs: &ShareAttrs) -> Result<Option<serde_json::Value>, StoreError> {
    let canonical = ShareAttrs {
        description: attrs.description.clone(),
        filters: attrs
            .filter_specs()?
            .iter()
            .map(ToString::to_string)
            .collect(),
    };
    if canonical == ShareAttrs::default() {
        return Ok(None);
    }
    serde_json::to_value(&canonical)
        .map(Some)
        .map_err(|e| StoreError::Parse(format!("share attrs encode: {e}")))
}

fn members(object: &Object) -> Vec<String> {
    object
        .tree
        .links
        .get(SESSIONS_LINK)
        .cloned()
        .unwrap_or_default()
}

fn join_nums(nums: &[u32]) -> String {
    nums.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{git_in, run};
    use crate::object::object_ref;
    use crate::session::SessionAttrsRecord;
    use crate::session::tests::{FakeDriver, fake, write_session_dir};
    use crate::test_support::open_store;
    use gage_core::uuid::new_uuid;
    use gage_session::SummaryAttrs;
    use std::path::Path;
    use std::slice;

    fn rev_parse(store: &Store, id: &str) -> String {
        store.rev_parse(&object_ref(id)).unwrap().unwrap()
    }

    /// A native session added through the fake driver; the stored id.
    fn native(store: &Store, id: &str) -> String {
        SessionStore::from(store)
            .add(&FakeDriver, &mut fake(id, &[("session.jsonl", "a\n")]))
            .unwrap()
            .id
    }

    /// A published session derived from `original`, recording the
    /// given filters as `(name, version)`; the stored id.
    fn published(
        root: &Path,
        store: &Store,
        original: &str,
        filters: &[(&str, Option<&str>)],
    ) -> String {
        let original_sha = rev_parse(store, original);
        let id = new_uuid();
        let attrs = SessionAttrsRecord {
            driver: "fake 0.1".into(),
            origin: format!("session:{original}@{original_sha}"),
            native_mtime: 0,
            native_size: 0,
            session_type: "fake".into(),
            content_format: "fake-lines 1".into(),
            size: 2,
            project: None,
            summary: SummaryAttrs::default(),
            publish: Some(PublishAttrs {
                filters: filters
                    .iter()
                    .map(|(name, version)| PublishFilter {
                        name: name.to_string(),
                        version: version.map(String::from),
                    })
                    .collect(),
            }),
        };
        let dir = write_session_dir(root, &id, &attrs, "[]\n", &[("session.jsonl", "a\n")]);
        SessionStore::from(store).create_from_dir(&dir).unwrap();
        id
    }

    fn with_filters(filters: &[&str]) -> ShareAttrs {
        ShareAttrs {
            description: None,
            filters: filters.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn member_ids(store: &Store, share: &str) -> Vec<String> {
        ShareStore::from(store)
            .sessions(share)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    #[test]
    fn filter_spec_parses_renders_and_matches() {
        let bare: FilterSpec = "secret".parse().unwrap();
        assert_eq!(bare.requirement, None);
        assert!(bare.accepts(None));
        assert!(bare.accepts(Some("0.1.0")));
        assert_eq!(bare.to_string(), "secret");

        let ranged: FilterSpec = "secret  >=1.2   <2".parse().unwrap();
        assert!(ranged.accepts(Some("1.2.0")));
        assert!(ranged.accepts(Some("1.9.3")));
        assert!(!ranged.accepts(Some("2.0.0")));
        assert!(!ranged.accepts(Some("1.1.9")));
        assert!(!ranged.accepts(None));
        assert!(!ranged.accepts(Some("not-a-version")));
        assert_eq!(ranged.to_string(), "secret >=1.2 <2");

        let caret: FilterSpec = "pii ^1".parse().unwrap();
        assert_eq!(caret.to_string(), "pii ^1");

        for bad in [
            "",
            "   ",
            "se cret >=1",
            "secret >=",
            "secret,pii",
            "secret 1.x.y",
        ] {
            let err = bad.parse::<FilterSpec>().unwrap_err();
            assert!(matches!(err, StoreError::FilterSpec(_)), "{bad:?}: {err}");
        }
    }

    #[test]
    fn filter_specs_split_on_commas_and_keep_comparators_together() {
        let specs = parse_filter_specs("secret >=1.2 <2, language,").unwrap();
        let text: Vec<String> = specs.iter().map(ToString::to_string).collect();
        assert_eq!(text, ["secret >=1.2 <2", "language"]);
        assert!(parse_filter_specs("").unwrap().is_empty());
        assert!(matches!(
            parse_filter_specs("secret, >=1").unwrap_err(),
            StoreError::FilterSpec(_)
        ));
    }

    #[test]
    fn create_link_unlink_and_list_members_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let shares = ShareStore::from(&store);
        let o = native(&store, "o");
        let p1 = published(tmp.path(), &store, &o, &[("secret", Some("1.0.0"))]);
        let p2 = published(tmp.path(), &store, &o, &[("secret", Some("1.0.0"))]);
        let p3 = published(tmp.path(), &store, &o, &[("secret", Some("1.0.0"))]);

        let attrs = ShareAttrs {
            description: Some("Work.".into()),
            filters: vec!["secret".into()],
        };
        let share = shares.create(&attrs, slice::from_ref(&p1)).unwrap();
        let record = shares.get(&share).unwrap();
        assert_eq!(record.attrs, attrs);
        assert_eq!(record.session_count, 1);
        assert_eq!(shares.iter().unwrap().count(), 1);

        let outcomes = shares.link(&share, &[p2.clone(), p1.clone()]).unwrap();
        assert_eq!(
            outcomes,
            [
                ShareLinkOutcome {
                    session_num: 2,
                    id: p2.clone(),
                    outcome: SessionOutcome::Added
                },
                ShareLinkOutcome {
                    session_num: 1,
                    id: p1.clone(),
                    outcome: SessionOutcome::Unchanged
                },
            ]
        );
        shares.link(&share, slice::from_ref(&p3)).unwrap();
        assert_eq!(
            member_ids(&store, &share),
            [p1.clone(), p2.clone(), p3.clone()]
        );

        let unlinked = shares.unlink(&share, slice::from_ref(&p2)).unwrap();
        assert_eq!(
            unlinked,
            [ShareUnlinkOutcome {
                session_num: 2,
                id: p2.clone()
            }]
        );
        assert_eq!(member_ids(&store, &share), [p1.clone(), p3.clone()]);
        let err = shares.unlink(&share, slice::from_ref(&p2)).unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)), "{err}");

        let held = shares.containing(&[p1.clone(), p2.clone()]).unwrap();
        assert_eq!(
            held,
            [ShareMembers {
                share_id: share.clone(),
                session_ids: vec![p1.clone()]
            }]
        );
    }

    #[test]
    fn membership_refuses_unpublished_and_underfiltered_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let shares = ShareStore::from(&store);
        let o = native(&store, "o");
        let old = published(tmp.path(), &store, &o, &[("secret", Some("1.1.0"))]);
        let unversioned = published(tmp.path(), &store, &o, &[("secret", None)]);
        let other = published(tmp.path(), &store, &o, &[("language", Some("1.0.0"))]);
        let good = published(
            tmp.path(),
            &store,
            &o,
            &[("secret", Some("1.4.0")), ("language", Some("1.0.0"))],
        );
        let attrs = with_filters(&["secret >=1.2", "language"]);

        let reason = |ids: &[String]| match shares.create(&attrs, ids).unwrap_err() {
            StoreError::ShareMembership { session, reason } => (session, reason),
            e => panic!("{e}"),
        };
        assert_eq!(
            reason(slice::from_ref(&o)),
            (o.clone(), "not a published session".to_string())
        );
        assert_eq!(
            reason(slice::from_ref(&other)),
            (other.clone(), "published without filter secret".to_string())
        );
        assert_eq!(
            reason(slice::from_ref(&old)),
            (
                old.clone(),
                "filter secret at version 1.1.0 does not satisfy secret >=1.2".to_string()
            )
        );
        assert_eq!(
            reason(slice::from_ref(&unversioned)),
            (
                unversioned.clone(),
                "filter secret at version none does not satisfy secret >=1.2".to_string()
            )
        );
        // A refused create writes nothing
        assert_eq!(shares.iter().unwrap().count(), 0);

        let share = shares.create(&attrs, slice::from_ref(&good)).unwrap();
        let before = rev_parse(&store, &share);
        let err = shares.link(&share, &[good.clone(), o.clone()]).unwrap_err();
        assert!(matches!(err, StoreError::ShareMembership { .. }), "{err}");
        assert_eq!(rev_parse(&store, &share), before);

        // A bare name accepts a filter recorded without a version
        let lenient = shares
            .create(&with_filters(&["secret"]), slice::from_ref(&unversioned))
            .unwrap();
        assert_eq!(member_ids(&store, &lenient), [unversioned]);
    }

    #[test]
    fn accepts_applies_the_filter_rule_to_a_publish_record() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let shares = ShareStore::from(&store);
        let share = shares
            .create(&with_filters(&["secret >=1.2", "language"]), &[])
            .unwrap();
        let record = |filters: &[(&str, &str)]| PublishAttrs {
            filters: filters
                .iter()
                .map(|(name, version)| PublishFilter {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                })
                .collect(),
        };
        shares
            .accepts(
                &share,
                "pending",
                &record(&[("secret", "1.2.0"), ("language", "1.0.0")]),
            )
            .unwrap();
        let err = shares
            .accepts(&share, "pending", &record(&[("secret", "1.2.0")]))
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::ShareMembership { session, reason }
                if session == "pending" && reason == "published without filter language"),
            "{err}"
        );
    }

    #[test]
    fn edit_refuses_a_tightened_spec_unless_forced_which_unlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let shares = ShareStore::from(&store);
        let o = native(&store, "o");
        let old = published(tmp.path(), &store, &o, &[("secret", Some("1.0.0"))]);
        let new = published(tmp.path(), &store, &o, &[("secret", Some("1.3.0"))]);
        let share = shares
            .create(&with_filters(&["secret"]), &[old.clone(), new.clone()])
            .unwrap();
        let before = rev_parse(&store, &share);

        let tightened = ShareAttrs {
            description: Some("Newer only.".into()),
            filters: vec!["secret >=1.2".into()],
        };
        let err = shares.edit(&share, &tightened, false).unwrap_err();
        assert!(
            matches!(&err, StoreError::ShareMembership { session, .. } if *session == old),
            "{err}"
        );
        assert_eq!(rev_parse(&store, &share), before);

        let outcome = shares.edit(&share, &tightened, true).unwrap();
        assert_eq!(outcome.unlinked, [old]);
        let record = shares.get(&share).unwrap();
        assert_eq!(record.attrs, tightened);
        assert_eq!(member_ids(&store, &share), [new]);

        // An edit that every member satisfies changes only the attrs
        let loosened = with_filters(&["secret"]);
        assert_eq!(
            shares.edit(&share, &loosened, false).unwrap(),
            ShareEditOutcome::default()
        );
        assert_eq!(shares.get(&share).unwrap().attrs, loosened);
    }

    #[test]
    fn delete_keeps_members_and_cascade_removes_the_unheld_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let shares = ShareStore::from(&store);
        let sessions = SessionStore::from(&store);
        let o = native(&store, "o");
        let alone = published(tmp.path(), &store, &o, &[]);
        let in_share = published(tmp.path(), &store, &o, &[]);
        let in_dataset = published(tmp.path(), &store, &o, &[]);
        let gone = published(tmp.path(), &store, &o, &[]);

        let plain = shares
            .create(&ShareAttrs::default(), slice::from_ref(&alone))
            .unwrap();
        let deleted = shares.delete(&plain).unwrap();
        assert_eq!(deleted, plain);
        assert!(sessions.get(&alone).is_ok());
        assert!(matches!(
            shares.get(&plain).unwrap_err(),
            StoreError::ObjectDeleted(_)
        ));

        let share = shares
            .create(
                &ShareAttrs::default(),
                &[
                    alone.clone(),
                    in_share.clone(),
                    in_dataset.clone(),
                    gone.clone(),
                ],
            )
            .unwrap();
        shares
            .create(&ShareAttrs::default(), slice::from_ref(&in_share))
            .unwrap();
        let datasets = DatasetStore::from(&store);
        let dataset = datasets.create().unwrap();
        datasets
            .sessions_link(&dataset, slice::from_ref(&in_dataset))
            .unwrap();
        sessions.tombstone(&sessions.get(&gone).unwrap()).unwrap();

        let outcome = shares.delete_cascade(&share).unwrap();
        assert_eq!(
            outcome,
            ShareDeleted {
                share: share.clone(),
                removed: vec![alone.clone()],
                kept: vec![in_share.clone(), in_dataset.clone()],
            }
        );
        assert!(matches!(
            sessions.get(&alone).unwrap_err(),
            StoreError::ObjectDeleted(_)
        ));
        assert!(sessions.get(&in_share).is_ok());
        assert!(sessions.get(&in_dataset).is_ok());
    }

    /// The exact tree a share writes: no `attrs.json` without a
    /// description or filters, one with, and the filter entries in
    /// canonical text. A change here is a change to the stored format
    /// and is reviewed as one.
    #[test]
    fn layout_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let shares = ShareStore::from(&store);
        let names = |sha: &str| -> Vec<String> {
            store.ls(sha).unwrap().into_iter().map(|e| e.name).collect()
        };
        let cat_file = |spec: &str| run(git_in(store.path(), ["cat-file", "-p", spec])).unwrap();

        let plain = shares.create(&ShareAttrs::default(), &[]).unwrap();
        let plain_sha = rev_parse(&store, &plain);
        assert_eq!(names(&plain_sha), ["created", "id", "modified", "type"]);
        assert_eq!(cat_file(&format!("{plain_sha}:type")), "gage::share 1\n");

        let o = native(&store, "o");
        let p1 = published(tmp.path(), &store, &o, &[("secret", Some("1.2.0"))]);
        let p1_sha = rev_parse(&store, &p1);
        let id = shares
            .create(
                &ShareAttrs {
                    description: Some("Sessions shared for work.".into()),
                    filters: vec!["secret   >=1.2,   <2".replace(',', "")],
                },
                &[],
            )
            .unwrap();
        shares.link(&id, slice::from_ref(&p1)).unwrap();
        let sha = rev_parse(&store, &id);
        assert_eq!(
            names(&sha),
            [
                "attrs.json",
                "created",
                "first-parent",
                "id",
                "modified",
                "sessions.link",
                "type"
            ]
        );
        assert_eq!(
            cat_file(&format!("{sha}:attrs.json")),
            "{\"description\":\"Sessions shared for work.\",\"filters\":[\"secret >=1.2 <2\"]}\n"
        );
        assert_eq!(
            cat_file(&format!("{sha}:sessions.link")),
            format!("{p1_sha}\n")
        );
    }
}
