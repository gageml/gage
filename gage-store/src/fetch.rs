//! Fetch: adopting what a remote has into the store.
//!
//! Git applies Gage's ref rules on push, through the receiving
//! store's `update` hook, and none of them on fetch. So a fetch is two
//! steps. One forced `git fetch` writes what the remote has under its
//! tracking prefix, the real layout with `refs/gage/` replaced by
//! `refs/remotes/<name>/gage/` for a configured remote or by a
//! transient `refs/gage-fetch/<run>/` for a bare URL, where nothing
//! carries a rule. Then [`Store::apply_fetched`] decides per tracked
//! ref what the local ref becomes, with the `update` hook's rules in
//! Rust plus the merges a hook cannot do, and reports what it did.
//!
//! Object refs are decided first, then the key refs of the adopted
//! objects are derived from their `attrs.key` (the remote's key refs
//! are not consulted: a key ref is derived state), then tag refs.
//!
//! A divergence, two live commits of one object neither of which
//! descends from the other, is merged per type where the type allows
//! it: an issue unions its change entries and keeps the one status
//! that moved; a dataset merges membership as a set against the
//! common ancestor; a session takes the longer content when one is a
//! byte prefix of the other. Every merge is one new local commit whose
//! `first-parent` is the remote's commit, so the next push is a
//! fast-forward. What cannot merge is a conflict: reported, the local
//! ref left alone, the remote's commit still readable under its
//! tracking ref. `--ours` and `--theirs` resolve a named conflict.

use std::collections::{BTreeMap, HashSet};

use crate::dataset::{ATTACHMENTS_LINK, OBJECT_TYPE as DATASET_TYPE, SESSIONS_LINK};
use crate::git::{git_in, run};
use crate::issue::{IssueStore, OBJECT_TYPE as ISSUE_TYPE};
use crate::key::plan_key_ref;
use crate::object::{Object, ObjectTree};
use crate::refs::{OBJECT_REFS, TAG_REFS};
use crate::remote::FetchSource;
use crate::session::OBJECT_TYPE as SESSION_TYPE;
use crate::{Store, StoreError};

/// Which side a resolution keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// What [`Store::apply_fetched`] did with one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No local ref existed; the remote's commit is adopted
    Created,
    /// Both sides hold the same commit, or both hold tombstones
    UpToDate,
    /// The local commit descends from the remote's, or a local
    /// tombstone stands against a remote edit; nothing to adopt
    Ahead,
    /// The remote's commit descends from the local one; adopted
    FastForwarded,
    /// The remote's tombstone replaces the live local commit
    Tombstoned,
    /// The remote's live commit replaces the local tombstone
    Resurrected,
    /// Divergent commits merged into one new local commit
    Merged,
    /// A divergence resolved by the named side
    Resolved(Side),
    /// A divergence nothing could merge; the local ref is untouched
    Conflict(Conflict),
}

/// What kept a divergence from merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Both sides changed an issue's status to different results
    Status { ours: String, theirs: String },
    /// Both sides changed an object the type merges as a whole, or no
    /// common ancestor exists
    Object,
    /// A dataset member linked at two commits where neither descends
    /// from the other
    Member { id: String },
    /// A tag naming different objects on each side
    Tag,
    /// A fetched object's key is held by another live local object
    Key { held_by: String },
}

/// One ref's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefOutcome {
    pub ref_name: String,
    /// The object id the ref concerns
    pub id: String,
    /// The bare type name: `issue`, `tag` for a tag ref, `key` for a
    /// key ref
    pub kind: String,
    pub outcome: Outcome,
}

/// Everything one fetch did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FetchReport {
    pub outcomes: Vec<RefOutcome>,
}

impl FetchReport {
    /// The outcomes that are conflicts, in report order.
    pub fn conflicts(&self) -> impl Iterator<Item = &RefOutcome> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.outcome, Outcome::Conflict(_)))
    }
}

impl Store {
    /// Decide and write the local refs from the tracking refs of
    /// `source`: every tracked object and tag ref, or only those whose
    /// real ref name is in `only`. `author` signs any resolution entry
    /// a merge writes; `choose` resolves divergences that would
    /// otherwise be conflicts. The index is brought up to date with
    /// every adopted commit.
    pub fn apply_fetched(
        &self,
        source: &FetchSource,
        only: Option<&HashSet<String>>,
        author: &str,
        choose: Option<Side>,
    ) -> Result<FetchReport, StoreError> {
        let tracked = self.tracking_refs(source)?;
        let selected = |name: &str| only.is_none_or(|set| set.contains(name));
        let mut report = FetchReport::default();

        let mut adopted: Vec<(String, String)> = Vec::new();
        for (ref_name, remote_sha) in &tracked {
            if !ref_name.starts_with(OBJECT_REFS) || !selected(ref_name) {
                continue;
            }
            let outcome = self.apply_object(ref_name, remote_sha, source.name(), author, choose)?;
            if matches!(
                outcome.outcome,
                Outcome::Created
                    | Outcome::FastForwarded
                    | Outcome::Resurrected
                    | Outcome::Merged
                    | Outcome::Resolved(_)
            ) {
                adopted.push((outcome.id.clone(), ref_name.clone()));
            }
            report.outcomes.push(outcome);
        }

        for (id, ref_name) in adopted {
            if let Some(outcome) = self.derive_key_ref(&id, &ref_name)? {
                report.outcomes.push(outcome);
            }
        }

        for (ref_name, remote_sha) in &tracked {
            if !ref_name.starts_with(TAG_REFS) || !selected(ref_name) {
                continue;
            }
            report
                .outcomes
                .push(self.apply_tag(ref_name, remote_sha, choose)?);
        }
        Ok(report)
    }

    /// The tracking refs of `source`, keyed by the real ref name they
    /// mirror.
    pub fn tracking_refs(
        &self,
        source: &FetchSource,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let prefix = source.tracking();
        let out = run(git_in(
            self.path(),
            ["for-each-ref", "--format=%(refname) %(objectname)", prefix],
        ))?;
        let mut refs = BTreeMap::new();
        for line in out.lines() {
            let (tracking, sha) = line
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
            let rest = tracking.strip_prefix(prefix).ok_or_else(|| {
                StoreError::Parse(format!("tracking ref outside prefix: {tracking}"))
            })?;
            refs.insert(format!("{}{rest}", crate::refs::ROOT), sha.to_string());
        }
        Ok(refs)
    }

    fn apply_object(
        &self,
        ref_name: &str,
        remote_sha: &str,
        remote_name: &str,
        author: &str,
        choose: Option<Side>,
    ) -> Result<RefOutcome, StoreError> {
        let remote = self.read_object(remote_sha)?;
        let kind = type_name(&remote.header.object_type).to_string();
        let id = remote.header.id.clone();
        let done = |outcome| {
            Ok(RefOutcome {
                ref_name: ref_name.to_string(),
                id: id.clone(),
                kind: kind.clone(),
                outcome,
            })
        };
        let Some(local_sha) = self.rev_parse(ref_name)? else {
            self.record_ref_move(&remote, "")?;
            return done(Outcome::Created);
        };
        if local_sha == remote_sha {
            return done(Outcome::UpToDate);
        }
        let remote_chain = self.walk_parent_chain(remote_sha)?;
        if remote_chain.iter().any(|c| c == &local_sha) {
            self.record_ref_move(&remote, &local_sha)?;
            return done(Outcome::FastForwarded);
        }
        let local_chain = self.walk_parent_chain(&local_sha)?;
        if local_chain.iter().any(|c| c == remote_sha) {
            return done(Outcome::Ahead);
        }
        let local = self.read_object(&local_sha)?;
        let same_identity = remote.header.first_parent.is_none()
            && remote.header.object_type == local.header.object_type
            && remote.header.created_ms == local.header.created_ms;
        match (local.header.is_tombstone(), remote.header.is_tombstone()) {
            (true, true) => done(Outcome::UpToDate),
            (false, true) if same_identity => {
                self.record_ref_move(&remote, &local_sha)?;
                done(Outcome::Tombstoned)
            }
            (false, true) => done(Outcome::Conflict(Conflict::Object)),
            (true, false) if same_identity => {
                self.record_ref_move(&remote, &local_sha)?;
                done(Outcome::Resurrected)
            }
            // A local tombstone stands against a remote edit
            (true, false) => done(Outcome::Ahead),
            (false, false) => {
                let remote_set: HashSet<&str> = remote_chain.iter().map(String::as_str).collect();
                let base_sha = local_chain
                    .iter()
                    .find(|c| remote_set.contains(c.as_str()))
                    .cloned();
                let base = match &base_sha {
                    Some(sha) => Some(self.read_object(sha)?),
                    None => None,
                };
                let outcome =
                    self.merge(&local, &remote, base.as_ref(), remote_name, author, choose)?;
                done(outcome)
            }
        }
    }

    /// Merge two live divergent commits of one object, or resolve them
    /// by `choose`. Writes the result and returns the outcome.
    fn merge(
        &self,
        local: &Object,
        remote: &Object,
        base: Option<&Object>,
        remote_name: &str,
        author: &str,
        choose: Option<Side>,
    ) -> Result<Outcome, StoreError> {
        let message = |what: &str| {
            format!(
                "{} {what}: {}",
                type_name(&remote.header.object_type),
                remote.header.id
            )
        };
        let object_type = remote.header.object_type.as_str();
        if object_type == ISSUE_TYPE {
            let merged = IssueStore::from(self).merge_divergent(
                local,
                remote,
                base,
                choose,
                author,
                remote_name,
            )?;
            return match merged {
                Ok(tree) => {
                    self.edit_onto(remote, &tree, &message("merge"), &local.commit_sha)?;
                    Ok(match choose {
                        Some(side) => Outcome::Resolved(side),
                        None => Outcome::Merged,
                    })
                }
                Err((ours, theirs)) => Ok(Outcome::Conflict(Conflict::Status { ours, theirs })),
            };
        }
        if let Some(side) = choose {
            return self.resolve_whole(local, remote, side, &message("resolve"));
        }
        if object_type == DATASET_TYPE {
            return match self.merge_dataset(local, remote, base)? {
                Ok(tree) => {
                    self.edit_onto(remote, &tree, &message("merge"), &local.commit_sha)?;
                    Ok(Outcome::Merged)
                }
                Err(None) => Ok(Outcome::Conflict(Conflict::Object)),
                Err(Some(id)) => Ok(Outcome::Conflict(Conflict::Member { id })),
            };
        }
        if object_type == SESSION_TYPE {
            return match self.session_extension(local, remote)? {
                Some(side) => {
                    self.resolve_whole(local, remote, side, &message("merge"))?;
                    Ok(Outcome::Merged)
                }
                None => Ok(Outcome::Conflict(Conflict::Object)),
            };
        }
        Ok(Outcome::Conflict(Conflict::Object))
    }

    /// Keep one side of a divergence as a whole: ours is the local
    /// tree rewritten onto the remote's commit, theirs is the remote's
    /// commit adopted as is.
    fn resolve_whole(
        &self,
        local: &Object,
        remote: &Object,
        side: Side,
        message: &str,
    ) -> Result<Outcome, StoreError> {
        match side {
            Side::Ours => {
                self.edit_onto(remote, &local.tree, message, &local.commit_sha)?;
            }
            Side::Theirs => self.record_ref_move(remote, &local.commit_sha)?,
        }
        Ok(Outcome::Resolved(side))
    }

    /// Set-merge a dataset's membership against the common ancestor:
    /// a member added on either side is in, one removed on either side
    /// is out, in ancestor order then local then remote additions. A
    /// member linked at two commits takes the descendant; neither
    /// descending is `Err(Some(member id))`; no ancestor is
    /// `Err(None)`.
    fn merge_dataset(
        &self,
        local: &Object,
        remote: &Object,
        base: Option<&Object>,
    ) -> Result<Result<ObjectTree, Option<String>>, StoreError> {
        let Some(base) = base else {
            return Ok(Err(None));
        };
        let mut tree = ObjectTree::default();
        for link in [SESSIONS_LINK, ATTACHMENTS_LINK] {
            let base_members = self.members(base, link)?;
            let local_members = self.members(local, link)?;
            let remote_members = self.members(remote, link)?;
            let local_by: BTreeMap<&str, &str> = local_members
                .iter()
                .map(|(id, sha)| (id.as_str(), sha.as_str()))
                .collect();
            let remote_by: BTreeMap<&str, &str> = remote_members
                .iter()
                .map(|(id, sha)| (id.as_str(), sha.as_str()))
                .collect();
            let base_ids: HashSet<&str> = base_members.iter().map(|(id, _)| id.as_str()).collect();
            let mut merged: Vec<(String, String)> = Vec::new();
            for (id, _) in &base_members {
                let (Some(l), Some(r)) = (local_by.get(id.as_str()), remote_by.get(id.as_str()))
                else {
                    continue;
                };
                match self.descendant(l, r)? {
                    Some(sha) => merged.push((id.clone(), sha)),
                    None => return Ok(Err(Some(id.clone()))),
                }
            }
            for (id, sha) in local_members.iter().chain(remote_members.iter()) {
                if base_ids.contains(id.as_str()) {
                    continue;
                }
                match merged.iter_mut().find(|(m, _)| m == id) {
                    None => merged.push((id.clone(), sha.clone())),
                    Some((_, held)) => match self.descendant(held, sha)? {
                        Some(winner) => *held = winner,
                        None => return Ok(Err(Some(id.clone()))),
                    },
                }
            }
            if !merged.is_empty() {
                tree.links.insert(
                    link.to_string(),
                    merged.into_iter().map(|(_, sha)| sha).collect(),
                );
            }
        }
        Ok(Ok(tree))
    }

    /// The `(object id, commit)` members a link file of `object` lists.
    fn members(&self, object: &Object, link: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut out = Vec::new();
        for sha in object.tree.links.get(link).into_iter().flatten() {
            out.push((self.read_header(sha)?.id, sha.clone()));
        }
        Ok(out)
    }

    /// Of two commits of one object, the one that descends from the
    /// other, or `None` when neither does.
    fn descendant(&self, a: &str, b: &str) -> Result<Option<String>, StoreError> {
        if a == b {
            return Ok(Some(a.to_string()));
        }
        if self.walk_parent_chain(a)?.iter().any(|c| c == b) {
            return Ok(Some(a.to_string()));
        }
        if self.walk_parent_chain(b)?.iter().any(|c| c == a) {
            return Ok(Some(b.to_string()));
        }
        Ok(None)
    }

    /// Which side's session content extends the other's: `Theirs`
    /// when every local file is a byte prefix of the remote's, `Ours`
    /// for the reverse, `None` when neither holds.
    fn session_extension(
        &self,
        local: &Object,
        remote: &Object,
    ) -> Result<Option<Side>, StoreError> {
        let local_files = self.session_files(local)?;
        let remote_files = self.session_files(remote)?;
        if self.files_extend(&local_files, &remote_files)? {
            return Ok(Some(Side::Theirs));
        }
        if self.files_extend(&remote_files, &local_files)? {
            return Ok(Some(Side::Ours));
        }
        Ok(None)
    }

    /// The blobs under `files.d` of a session commit, by path.
    fn session_files(&self, object: &Object) -> Result<BTreeMap<String, String>, StoreError> {
        let mut files = BTreeMap::new();
        if let Some(sha) = object.tree.subtrees.get("files.d") {
            self.walk_tree(sha, "", &mut |entry| {
                if entry.kind == crate::git::EntryKind::Blob {
                    files.insert(entry.name, entry.sha);
                }
                Ok(())
            })?;
        }
        Ok(files)
    }

    /// True when every file in `shorter` exists in `longer` with the
    /// shorter's bytes as a prefix of the longer's.
    fn files_extend(
        &self,
        shorter: &BTreeMap<String, String>,
        longer: &BTreeMap<String, String>,
    ) -> Result<bool, StoreError> {
        for (path, sha) in shorter {
            let Some(other) = longer.get(path) else {
                return Ok(false);
            };
            if sha == other {
                continue;
            }
            let a = self.read_blob_bytes(sha)?;
            let b = self.read_blob_bytes(other)?;
            if !b.starts_with(&a) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Claim the key ref of an adopted object from its current tip, as
    /// a create does. A key held by another live object is reported
    /// and the object stays live without it.
    fn derive_key_ref(&self, id: &str, ref_name: &str) -> Result<Option<RefOutcome>, StoreError> {
        let Some(tip) = self.rev_parse(ref_name)? else {
            return Ok(None);
        };
        let object = self.read_object(&tip)?;
        if object.header.is_tombstone() {
            return Ok(None);
        }
        let planned = plan_key_ref(
            self,
            None,
            &object.header.object_type,
            id,
            Some(&object.tree),
        );
        match planned {
            Ok(Some(update)) => {
                if self.rev_parse(&update.ref_name)?.as_deref() != Some(tip.as_str()) {
                    run(git_in(
                        self.path(),
                        ["update-ref", &update.ref_name, &tip, &update.old],
                    ))?;
                }
                Ok(None)
            }
            Ok(None) => Ok(None),
            Err(StoreError::KeyTaken { key, id: held_by }) => Ok(Some(RefOutcome {
                ref_name: format!("{}/{key}", type_name(&object.header.object_type)),
                id: id.to_string(),
                kind: "key".to_string(),
                outcome: Outcome::Conflict(Conflict::Key { held_by }),
            })),
            Err(e) => Err(e),
        }
    }

    fn apply_tag(
        &self,
        ref_name: &str,
        remote_sha: &str,
        choose: Option<Side>,
    ) -> Result<RefOutcome, StoreError> {
        let remote_id = self.read_header(remote_sha)?.id;
        let done = |outcome| {
            Ok(RefOutcome {
                ref_name: ref_name.to_string(),
                id: remote_id.clone(),
                kind: "tag".to_string(),
                outcome,
            })
        };
        let Some(local_sha) = self.rev_parse(ref_name)? else {
            run(git_in(
                self.path(),
                ["update-ref", ref_name, remote_sha, ""],
            ))?;
            return done(Outcome::Created);
        };
        if local_sha == remote_sha {
            return done(Outcome::UpToDate);
        }
        let local_id = self.read_header(&local_sha)?.id;
        if local_id == remote_id {
            if self
                .walk_parent_chain(remote_sha)?
                .iter()
                .any(|c| c == &local_sha)
            {
                run(git_in(
                    self.path(),
                    ["update-ref", ref_name, remote_sha, &local_sha],
                ))?;
                return done(Outcome::FastForwarded);
            }
            return done(Outcome::Ahead);
        }
        match choose {
            Some(Side::Theirs) => {
                run(git_in(
                    self.path(),
                    ["update-ref", ref_name, remote_sha, &local_sha],
                ))?;
                done(Outcome::Resolved(Side::Theirs))
            }
            Some(Side::Ours) => done(Outcome::Resolved(Side::Ours)),
            None => done(Outcome::Conflict(Conflict::Tag)),
        }
    }
}

/// `issue` for `gage::issue`.
fn type_name(object_type: &str) -> &str {
    object_type.strip_prefix("gage::").unwrap_or(object_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::{IssueInput, IssueStatus, StatusReason};
    use crate::note::{NoteEdit, NoteInput, NoteStore, NoteValue};
    use crate::refs::object_ref;
    use crate::remote::RemoteStore;
    use crate::test_support::open_store;
    use crate::{DatasetStore, TagStore};

    fn note(store: &Store, name: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text("v1".into()),
                author: "user:test",
                target: None,
                metadata: None,
                work_id: None,
            })
            .unwrap()
    }

    fn edit_note(store: &Store, id: &str, value: &str) {
        NoteStore::from(store)
            .edit(
                id,
                NoteEdit {
                    value: Some(NoteValue::Text(value.into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
    }

    fn issue(store: &Store, name: &str) -> String {
        IssueStore::from(store)
            .create(IssueInput {
                name,
                title: "t",
                description: None,
                author: "user:test",
                status: IssueStatus::Open,
                evidence: &[],
                key: None,
            })
            .unwrap()
    }

    fn tip(store: &Store, id: &str) -> String {
        store.rev_parse(&object_ref(id)).unwrap().unwrap()
    }

    /// Two stores, each a remote of the other. A test writes to either
    /// and fetches `a` into `b`.
    fn pair() -> (tempfile::TempDir, Store, Store) {
        let dir = tempfile::tempdir().unwrap();
        let (a, _fa) = open_store(&dir.path().join("a"));
        let (b, _fb) = open_store(&dir.path().join("b"));
        RemoteStore::from(&a)
            .add("b", b.path().to_str().unwrap(), true)
            .unwrap();
        RemoteStore::from(&b)
            .add("a", a.path().to_str().unwrap(), true)
            .unwrap();
        std::mem::forget(_fa);
        std::mem::forget(_fb);
        (dir, a, b)
    }

    fn fetch(store: &Store, name: &str, choose: Option<Side>) -> FetchReport {
        apply(store, &FetchSource::from_remote(name), choose)
    }

    fn apply(store: &Store, source: &FetchSource, choose: Option<Side>) -> FetchReport {
        let status = RemoteStore::from(store).fetch_all(source).unwrap();
        assert!(status.success());
        store
            .apply_fetched(source, None, "user:resolver", choose)
            .unwrap()
    }

    fn url(store: &Store) -> String {
        store.path().to_str().unwrap().to_string()
    }

    fn outcome<'a>(report: &'a FetchReport, id: &str) -> &'a Outcome {
        &report
            .outcomes
            .iter()
            .find(|o| o.id == id && o.kind != "key")
            .unwrap_or_else(|| panic!("no outcome for {id}: {report:?}"))
            .outcome
    }

    #[test]
    fn fetch_creates_fast_forwards_and_leaves_ahead_refs() {
        let (_dir, a, b) = pair();
        let n = note(&a, "n");
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &n), &Outcome::Created);
        assert_eq!(tip(&b, &n), tip(&a, &n));

        edit_note(&a, &n, "v2");
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &n), &Outcome::FastForwarded);
        assert_eq!(tip(&b, &n), tip(&a, &n));
        assert_eq!(outcome(&fetch(&b, "a", None), &n), &Outcome::UpToDate);

        edit_note(&b, &n, "v3");
        assert_eq!(outcome(&fetch(&b, "a", None), &n), &Outcome::Ahead);
    }

    #[test]
    fn url_fetch_writes_a_transient_namespace_and_discard_removes_it() {
        let (_dir, a, b) = pair();
        let n = note(&a, "n");
        let source = FetchSource::from_url(&url(&a));
        let remotes = RemoteStore::from(&b);
        let report = apply(&b, &source, None);
        assert_eq!(outcome(&report, &n), &Outcome::Created);
        assert_eq!(tip(&b, &n), tip(&a, &n));
        assert_eq!(b.tracking_refs(&source).unwrap().len(), 1);
        assert_eq!(remotes.list().unwrap().len(), 1, "no remote is added");

        assert_eq!(remotes.discard(&source).unwrap(), 1);
        assert!(b.tracking_refs(&source).unwrap().is_empty());
        assert_eq!(remotes.discard(&source).unwrap(), 0);
        assert_eq!(remotes.discard(&FetchSource::from_remote("a")).unwrap(), 0);
    }

    #[test]
    fn url_fetch_conflicts_resolve_on_a_later_run() {
        let (_dir, a, b) = pair();
        let n = note(&a, "n");
        apply(&b, &FetchSource::from_url(&url(&a)), None);
        edit_note(&a, &n, "theirs");
        edit_note(&b, &n, "ours");
        let report = apply(&b, &FetchSource::from_url(&url(&a)), None);
        assert_eq!(outcome(&report, &n), &Outcome::Conflict(Conflict::Object));

        let report = apply(&b, &FetchSource::from_url(&url(&a)), Some(Side::Theirs));
        assert_eq!(outcome(&report, &n), &Outcome::Resolved(Side::Theirs));
        assert_eq!(tip(&b, &n), tip(&a, &n));
    }

    #[test]
    fn clear_transient_removes_what_an_earlier_run_left() {
        let (_dir, a, b) = pair();
        note(&a, "n");
        let stale = FetchSource::from_url(&url(&a));
        apply(&b, &stale, None);
        let remotes = RemoteStore::from(&b);
        assert_eq!(remotes.clear_transient().unwrap(), 1);
        assert!(b.tracking_refs(&stale).unwrap().is_empty());
        assert_eq!(remotes.clear_transient().unwrap(), 0);
    }

    #[test]
    fn fetch_adopts_tombstones_and_resurrections_and_wins_over_edits() {
        let (_dir, a, b) = pair();
        let n = note(&a, "n");
        fetch(&b, "a", None);

        // Remote delete against a local edit: the tombstone wins
        edit_note(&b, &n, "local edit");
        NoteStore::from(&a).delete(&n).unwrap();
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &n), &Outcome::Tombstoned);
        assert!(b.read_header(&tip(&b, &n)).unwrap().is_tombstone());

        // Local delete against a remote edit: the local tombstone stays
        let m = note(&a, "m");
        fetch(&b, "a", None);
        NoteStore::from(&b).delete(&m).unwrap();
        edit_note(&a, &m, "remote edit");
        assert_eq!(outcome(&fetch(&b, "a", None), &m), &Outcome::Ahead);
    }

    #[test]
    fn divergent_notes_conflict_and_resolve_by_side() {
        let (_dir, a, b) = pair();
        let n = note(&a, "n");
        fetch(&b, "a", None);
        edit_note(&a, &n, "theirs");
        edit_note(&b, &n, "ours");
        let local_tip = tip(&b, &n);
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &n), &Outcome::Conflict(Conflict::Object));
        assert_eq!(tip(&b, &n), local_tip, "a conflict leaves the local ref");

        let report = fetch(&b, "a", Some(Side::Ours));
        assert_eq!(outcome(&report, &n), &Outcome::Resolved(Side::Ours));
        let merged = b.read_object(&tip(&b, &n)).unwrap();
        assert_eq!(
            merged.header.first_parent.as_deref(),
            Some(tip(&a, &n).as_str())
        );
        assert_eq!(
            NoteStore::from(&b).get(&n).unwrap().value,
            NoteValue::Text("ours".into())
        );

        // Theirs adopts the remote commit as is
        let m = note(&a, "m");
        fetch(&b, "a", None);
        edit_note(&a, &m, "theirs");
        edit_note(&b, &m, "ours");
        let report = fetch(&b, "a", Some(Side::Theirs));
        assert_eq!(outcome(&report, &m), &Outcome::Resolved(Side::Theirs));
        assert_eq!(tip(&b, &m), tip(&a, &m));
    }

    #[test]
    fn divergent_issues_merge_comments_and_one_sided_status_changes() {
        let (_dir, a, b) = pair();
        let i = issue(&a, "i");
        fetch(&b, "a", None);
        let issues_a = IssueStore::from(&a);
        let issues_b = IssueStore::from(&b);
        issues_a.comment(&i, "user:a", "from a").unwrap();
        issues_b
            .set_status(
                &i,
                IssueStatus::Closed,
                Some(StatusReason::WontFix),
                "user:b",
                None,
            )
            .unwrap();
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &i), &Outcome::Merged);
        let full = issues_b.get(&i).unwrap();
        assert_eq!(full.status, IssueStatus::Closed);
        assert_eq!(full.status_reason, Some(StatusReason::WontFix));
        assert_eq!(full.changes.len(), 3, "{:?}", full.changes);
        assert_eq!(full.commit_sha, tip(&b, &i));
        assert_eq!(
            b.read_header(&full.commit_sha)
                .unwrap()
                .first_parent
                .as_deref(),
            Some(tip(&a, &i).as_str())
        );
    }

    #[test]
    fn divergent_issue_statuses_conflict_and_resolve_with_a_recorded_entry() {
        let (_dir, a, b) = pair();
        let i = issue(&a, "i");
        fetch(&b, "a", None);
        IssueStore::from(&a)
            .set_status(
                &i,
                IssueStatus::Closed,
                Some(StatusReason::Completed),
                "user:a",
                None,
            )
            .unwrap();
        IssueStore::from(&b)
            .set_status(
                &i,
                IssueStatus::Closed,
                Some(StatusReason::WontFix),
                "user:b",
                None,
            )
            .unwrap();
        let report = fetch(&b, "a", None);
        assert_eq!(
            outcome(&report, &i),
            &Outcome::Conflict(Conflict::Status {
                ours: "closed/wontfix".into(),
                theirs: "closed/completed".into()
            })
        );
        let report = fetch(&b, "a", Some(Side::Theirs));
        assert_eq!(outcome(&report, &i), &Outcome::Resolved(Side::Theirs));
        let full = IssueStore::from(&b).get(&i).unwrap();
        assert_eq!(full.status_reason, Some(StatusReason::Completed));
        let last = full.changes.last().unwrap();
        assert_eq!(last.author, "user:resolver");
        assert_eq!(last.to_status, Some(IssueStatus::Closed));
        assert!(
            last.message
                .as_deref()
                .unwrap_or("")
                .contains("kept theirs")
        );
    }

    #[test]
    fn divergent_datasets_merge_membership_as_a_set() {
        let (_dir, a, b) = pair();
        let n1 = note(&a, "n1");
        let n2 = note(&a, "n2");
        let n3 = note(&a, "n3");
        let ds = DatasetStore::from(&a).create().unwrap();
        fetch(&b, "a", None);
        // Datasets link sessions; notes stand in through the generic
        // link writer, which the set merge does not care about
        link(&a, &ds, &[&n1, &n2]);
        fetch(&b, "a", None);
        link(&a, &ds, &[&n1, &n2, &n3]);
        link(&b, &ds, &[&n2]);
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &ds), &Outcome::Merged);
        let merged = b.read_object(&tip(&b, &ds)).unwrap();
        let members: Vec<String> = merged.tree.links[SESSIONS_LINK]
            .iter()
            .map(|sha| b.read_header(sha).unwrap().id)
            .collect();
        assert_eq!(members, [n2, n3]);
    }

    /// Rewrite a dataset's `sessions.link` to the tips of `ids`.
    fn link(store: &Store, ds: &str, ids: &[&str]) {
        let current = store.read_object(&tip(store, ds)).unwrap();
        let mut tree = ObjectTree::default();
        tree.links.insert(
            SESSIONS_LINK.to_string(),
            ids.iter().map(|id| tip(store, id)).collect(),
        );
        store.edit(&current, &tree, "dataset edit").unwrap();
    }

    #[test]
    fn tags_create_advance_and_conflict() {
        let (_dir, a, b) = pair();
        let n = note(&a, "n");
        let m = note(&a, "m");
        TagStore::from(&a).add("t", &n, false).unwrap();
        let report = fetch(&b, "a", None);
        let tag = report.outcomes.iter().find(|o| o.kind == "tag").unwrap();
        assert_eq!(tag.outcome, Outcome::Created);
        assert_eq!(
            b.rev_parse("refs/gage/1/tag/t").unwrap().unwrap(),
            tip(&a, &n)
        );

        TagStore::from(&a).add("t", &m, true).unwrap();
        let report = fetch(&b, "a", None);
        let tag = report.outcomes.iter().find(|o| o.kind == "tag").unwrap();
        assert_eq!(tag.outcome, Outcome::Conflict(Conflict::Tag));
        let report = fetch(&b, "a", Some(Side::Theirs));
        let tag = report.outcomes.iter().find(|o| o.kind == "tag").unwrap();
        assert_eq!(tag.outcome, Outcome::Resolved(Side::Theirs));
        assert_eq!(
            b.rev_parse("refs/gage/1/tag/t").unwrap().unwrap(),
            tip(&a, &m)
        );
    }

    #[test]
    fn a_fetched_key_is_derived_and_a_held_key_is_a_conflict() {
        let (_dir, a, b) = pair();
        let keyed = |store: &Store, title: &str| {
            IssueStore::from(store)
                .create(IssueInput {
                    name: "k",
                    title,
                    description: None,
                    author: "user:test",
                    status: IssueStatus::Open,
                    evidence: &[],
                    key: Some("shared"),
                })
                .unwrap()
        };
        let ia = keyed(&a, "a");
        let report = fetch(&b, "a", None);
        assert_eq!(outcome(&report, &ia), &Outcome::Created);
        assert_eq!(
            b.rev_parse("refs/gage/1/key/issue/shared")
                .unwrap()
                .unwrap(),
            tip(&b, &ia)
        );

        let (_dir2, c, d) = pair();
        let ic = keyed(&c, "c");
        let id_ = keyed(&d, "d");
        let report = fetch(&d, "a", None);
        assert_eq!(outcome(&report, &ic), &Outcome::Created);
        let key = report.outcomes.iter().find(|o| o.kind == "key").unwrap();
        assert_eq!(
            key.outcome,
            Outcome::Conflict(Conflict::Key {
                held_by: id_.clone()
            })
        );
        assert_eq!(
            d.rev_parse("refs/gage/1/key/issue/shared")
                .unwrap()
                .unwrap(),
            tip(&d, &id_)
        );
    }
}
