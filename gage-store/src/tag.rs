//! Tags: user-chosen names for objects.
//!
//! A tag is a ref `refs/gage/tag/<name>` pointing at a commit of the
//! object it names. The commit is the carrier of the object id: a
//! reader takes `id` from the commit's tree and resolves the object
//! from `refs/gage/object/<id>`, so a tag that lags its object, or
//! names a tombstoned object, still resolves. Which commit the ref
//! points at is not part of the tag's meaning.

use crate::git::{git_in, run};
use crate::index::IdMatch;
use crate::object::object_ref;
use crate::{Store, StoreError};

/// Full ref name of the tag with the given name.
pub(crate) fn tag_ref(name: &str) -> String {
    format!("refs/gage/tag/{name}")
}

/// Typed view over a [`Store`] for tags.
pub struct TagStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for TagStore<'a> {
    fn from(store: &'a Store) -> Self {
        TagStore { store }
    }
}

/// One tag with the current state of the object it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecord {
    pub name: String,
    /// The commit the tag ref points at.
    pub commit_sha: String,
    /// The named object's id.
    pub id: String,
    /// The named object's type at its current commit, e.g. `gage::note`.
    pub object_type: String,
    /// True when the object's current commit is a tombstone: the tag
    /// is dangling.
    pub deleted: bool,
    /// The object's `modified` marker at its current commit.
    pub modified_ms: Option<i64>,
}

/// Outcome of [`TagStore::add`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagAdded {
    pub name: String,
    /// The tagged object's id.
    pub id: String,
    /// The tagged object's type, e.g. `gage::note`.
    pub object_type: String,
    /// The commit the tag points at: the object's current commit.
    pub commit_sha: String,
    /// The id the tag named before a forced move, when it existed.
    pub previous_id: Option<String>,
}

/// A tag and the id of the object it names, read from the tagged
/// commit alone. The object's current state is not consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagTarget {
    pub name: String,
    /// The named object's id.
    pub id: String,
    /// The commit the tag ref points at.
    pub commit_sha: String,
}

/// One ref under `refs/gage/tag/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRef {
    pub name: String,
    /// Full ref name, e.g. `refs/gage/tag/<name>`.
    pub ref_name: String,
    /// The commit the tag points at.
    pub commit_sha: String,
}

impl TagStore<'_> {
    /// Every tag, name-sorted, with the commit each points at.
    pub fn list(&self) -> Result<Vec<TagRef>, StoreError> {
        let out = run(git_in(
            self.store.path(),
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/gage/tag/",
            ],
        ))?;
        let mut tags = Vec::new();
        for line in out.lines() {
            let (ref_name, sha) = line
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
            let Some(name) = ref_name.strip_prefix("refs/gage/tag/") else {
                continue;
            };
            tags.push(TagRef {
                name: name.to_string(),
                ref_name: ref_name.to_string(),
                commit_sha: sha.to_string(),
            });
        }
        Ok(tags)
    }

    /// Point `refs/gage/tag/<name>` at the current commit of the object
    /// `objectish` names. A name that exists is
    /// [`StoreError::TagExists`] unless `force`, which moves it. A
    /// tombstoned object cannot be tagged.
    pub fn add(&self, name: &str, objectish: &str, force: bool) -> Result<TagAdded, StoreError> {
        self.check_available(name, force)?;
        let found = self.store.resolve_in(objectish, None)?;
        if found.deleted {
            return Err(StoreError::ObjectDeleted(found.id));
        }
        let ref_name = tag_ref(name);
        let existing = self.store.rev_parse(&ref_name)?;
        let previous_id = match &existing {
            Some(sha) => {
                let id = self.store.read_header(sha)?.id;
                if !force {
                    return Err(StoreError::TagExists {
                        name: name.to_string(),
                        id,
                    });
                }
                Some(id)
            }
            None => None,
        };
        // The expected-old-value argument makes a concurrent add of
        // the same name fail rather than silently win; empty means
        // the ref must not exist yet
        let old = existing.as_deref().unwrap_or("");
        run(git_in(
            self.store.path(),
            ["update-ref", &ref_name, &found.tip_sha, old],
        ))?;
        Ok(TagAdded {
            name: name.to_string(),
            id: found.id,
            object_type: found.object_type,
            commit_sha: found.tip_sha,
            previous_id,
        })
    }

    /// Remove `refs/gage/tag/<name>`. The object the tag named is not
    /// touched. A name no tag has is [`StoreError::TagNotFound`].
    /// Returns the ref as it was.
    pub fn delete(&self, name: &str) -> Result<TagRef, StoreError> {
        let ref_name = tag_ref(name);
        let sha = self
            .store
            .rev_parse(&ref_name)?
            .ok_or_else(|| StoreError::TagNotFound(name.to_string()))?;
        // The expected-old-value argument makes a concurrent move of
        // the tag fail the delete rather than be silently discarded
        run(git_in(
            self.store.path(),
            ["update-ref", "-d", &ref_name, &sha],
        ))?;
        Ok(TagRef {
            name: name.to_string(),
            ref_name,
            commit_sha: sha,
        })
    }

    /// The object a tag names, or `None` when no tag has the name.
    /// The match is the object's current state, which may be a
    /// tombstone; the tag is then dangling.
    pub fn resolve(&self, name: &str) -> Result<Option<IdMatch>, StoreError> {
        let Some(sha) = self.store.rev_parse(&tag_ref(name))? else {
            return Ok(None);
        };
        let record = self.read(&TagRef {
            name: name.to_string(),
            ref_name: tag_ref(name),
            commit_sha: sha,
        })?;
        let tip = self
            .store
            .rev_parse(&object_ref(&record.id))?
            .ok_or_else(|| StoreError::ObjectNotFound(record.id.clone()))?;
        Ok(Some(IdMatch {
            id: record.id,
            tip_sha: tip,
            object_type: record.object_type,
            deleted: record.deleted,
        }))
    }

    /// Every tag in name order with the id of the object it names.
    /// One blob read per tag, the `id` entry of the tagged commit's
    /// tree; the object itself is not read. This is the reverse
    /// lookup from objects to their tags, done as one scan.
    pub fn targets(&self) -> Result<Vec<TagTarget>, StoreError> {
        self.list()?
            .into_iter()
            .map(|tag| {
                let bytes = self
                    .store
                    .read_blob_bytes(&format!("{}:id", tag.commit_sha))?;
                Ok(TagTarget {
                    name: tag.name,
                    id: String::from_utf8_lossy(&bytes).trim().to_string(),
                    commit_sha: tag.commit_sha,
                })
            })
            .collect()
    }

    /// Every tag in name order, each with the current state of the
    /// object it names. Reads three commits per tag where [`list`]
    /// reads none.
    ///
    /// [`list`]: TagStore::list
    pub fn records(&self) -> Result<Vec<TagRecord>, StoreError> {
        self.list()?.iter().map(|tag| self.read(tag)).collect()
    }

    /// The record of `tag`: the id from the tagged commit, the rest
    /// from the object's current commit.
    fn read(&self, tag: &TagRef) -> Result<TagRecord, StoreError> {
        let id = self.store.read_header(&tag.commit_sha)?.id;
        let tip = self
            .store
            .rev_parse(&object_ref(&id))?
            .ok_or_else(|| StoreError::ObjectNotFound(id.clone()))?;
        let header = self.store.read_header(&tip)?;
        Ok(TagRecord {
            name: tag.name.clone(),
            commit_sha: tag.commit_sha.clone(),
            id,
            deleted: header.is_tombstone(),
            object_type: header.object_type,
            modified_ms: header.modified_ms,
        })
    }

    /// Check that [`add`] would accept `name`: the name is valid and
    /// either no tag has it or `force` is set. A caller that creates
    /// the object to be tagged in the same operation checks first, so
    /// a bad tag leaves the store untouched.
    ///
    /// [`add`]: TagStore::add
    pub fn check_available(&self, name: &str, force: bool) -> Result<(), StoreError> {
        self.validate_name(name)?;
        if force {
            return Ok(());
        }
        match self.store.rev_parse(&tag_ref(name))? {
            Some(sha) => Err(StoreError::TagExists {
                name: name.to_string(),
                id: self.store.read_header(&sha)?.id,
            }),
            None => Ok(()),
        }
    }

    /// Git's ref name rules apply to a tag name. `check-ref-format`
    /// reports nothing on failure, so the error restates the rules.
    /// A leading `-` passes `check-ref-format` but reads as an option
    /// on any command line, which is why `git tag` refuses it too.
    fn validate_name(&self, name: &str) -> Result<(), StoreError> {
        if name.is_empty() || name.starts_with('-') {
            return Err(StoreError::TagName(name.to_string()));
        }
        match run(git_in(
            self.store.path(),
            ["check-ref-format", &tag_ref(name)],
        )) {
            Ok(_) => Ok(()),
            Err(StoreError::Git { status, .. }) if status.code() == Some(1) => {
                Err(StoreError::TagName(name.to_string()))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;
    use crate::{DatasetStore, NoteEdit, NoteInput, NoteStore, NoteValue};

    fn note(store: &Store, value: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name: "comment",
                value: NoteValue::Text(value.to_string()),
                author: "user:test",
                target: None,
                metadata: None,
                work_key: None,
            })
            .unwrap()
    }

    #[test]
    fn add_points_the_tag_ref_at_the_current_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        let (_, tip) = store.resolve_id(&id).unwrap();

        let added = TagStore::from(&store).add("baseline", &id, false).unwrap();

        assert_eq!(added.id, id);
        assert_eq!(added.object_type, "gage::note");
        assert_eq!(added.commit_sha, tip);
        assert_eq!(added.previous_id, None);
        assert_eq!(
            store.rev_parse("refs/gage/tag/baseline").unwrap(),
            Some(tip)
        );
    }

    #[test]
    fn list_returns_every_tag_name_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let tags = TagStore::from(&store);
        assert_eq!(tags.list().unwrap(), vec![]);

        let a = note(&store, "a");
        let b = note(&store, "b");
        let added_b = tags.add("beta", &b, false).unwrap();
        let added_a = tags.add("alpha/one", &a, false).unwrap();

        assert_eq!(
            tags.list().unwrap(),
            vec![
                TagRef {
                    name: "alpha/one".to_string(),
                    ref_name: "refs/gage/tag/alpha/one".to_string(),
                    commit_sha: added_a.commit_sha,
                },
                TagRef {
                    name: "beta".to_string(),
                    ref_name: "refs/gage/tag/beta".to_string(),
                    commit_sha: added_b.commit_sha,
                },
            ]
        );
    }

    #[test]
    fn add_accepts_a_unique_prefix_and_a_hierarchical_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");

        let added = TagStore::from(&store)
            .add("garrett/baseline", &id[..8], false)
            .unwrap();

        assert_eq!(added.id, id);
        assert!(
            store
                .rev_parse("refs/gage/tag/garrett/baseline")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn add_refuses_an_existing_name_unless_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let a = note(&store, "a");
        let b = note(&store, "b");
        let tags = TagStore::from(&store);
        tags.add("t", &a, false).unwrap();

        match tags.add("t", &b, false).unwrap_err() {
            StoreError::TagExists { name, id } => {
                assert_eq!(name, "t");
                assert_eq!(id, a);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(tags.resolve("t").unwrap().unwrap().id, a);

        let moved = tags.add("t", &b, true).unwrap();
        assert_eq!(moved.id, b);
        assert_eq!(moved.previous_id, Some(a));
        assert_eq!(tags.resolve("t").unwrap().unwrap().id, b);
    }

    #[test]
    fn check_available_reports_what_add_would_reject() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let tags = TagStore::from(&store);
        let id = note(&store, "v1");
        tags.add("taken", &id, false).unwrap();

        tags.check_available("free", false).unwrap();
        tags.check_available("taken", true).unwrap();
        match tags.check_available("taken", false).unwrap_err() {
            StoreError::TagExists { name, id: holder } => {
                assert_eq!(name, "taken");
                assert_eq!(holder, id);
            }
            other => panic!("unexpected error: {other}"),
        }
        match tags.check_available("a b", true).unwrap_err() {
            StoreError::TagName(name) => assert_eq!(name, "a b"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn add_rejects_names_git_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        let tags = TagStore::from(&store);

        for bad in [
            "", "a b", "a..b", "-x", "x.lock", "a/", ".hidden", "a^b", "a:b",
        ] {
            match tags.add(bad, &id, false).unwrap_err() {
                StoreError::TagName(name) => assert_eq!(name, bad),
                other => panic!("{bad:?}: unexpected error: {other}"),
            }
        }
        assert!(store.rev_parse("refs/gage/tag/").unwrap().is_none());
    }

    #[test]
    fn add_rejects_a_tombstoned_object() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        NoteStore::from(&store).delete(&id).unwrap();

        match TagStore::from(&store).add("t", &id, false).unwrap_err() {
            StoreError::ObjectDeleted(deleted) => assert_eq!(deleted, id),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn a_lagging_tag_resolves_to_the_current_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        let tags = TagStore::from(&store);
        let added = tags.add("t", &id, false).unwrap();

        NoteStore::from(&store)
            .edit(
                &id,
                NoteEdit {
                    value: Some(NoteValue::Text("v2".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
        let (_, tip) = store.resolve_id(&id).unwrap();
        assert_ne!(tip, added.commit_sha);

        let found = store.resolve_in("t", None).unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.tip_sha, tip);
        assert!(!found.deleted);
    }

    #[test]
    fn a_tag_on_a_deleted_object_resolves_as_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        TagStore::from(&store).add("t", &id, false).unwrap();
        NoteStore::from(&store).delete(&id).unwrap();

        let found = store.resolve_in("t", None).unwrap();
        assert_eq!(found.id, id);
        assert!(found.deleted);
    }

    #[test]
    fn a_tag_name_wins_over_an_id_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let a = note(&store, "a");
        let b = note(&store, "b");
        let prefix = a[..6].to_string();
        TagStore::from(&store).add(&prefix, &b, false).unwrap();

        assert_eq!(store.resolve_id(&prefix).unwrap().0, b);
        TagStore::from(&store).delete(&prefix).unwrap();
        assert_eq!(store.resolve_id(&prefix).unwrap().0, a);
    }

    #[test]
    fn delete_removes_the_ref_and_leaves_the_object() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let tags = TagStore::from(&store);
        let id = note(&store, "v1");
        let added = tags.add("t", &id, false).unwrap();
        tags.add("keep", &id, false).unwrap();

        let deleted = tags.delete("t").unwrap();

        assert_eq!(deleted.name, "t");
        assert_eq!(deleted.ref_name, "refs/gage/tag/t");
        assert_eq!(deleted.commit_sha, added.commit_sha);
        assert_eq!(store.rev_parse("refs/gage/tag/t").unwrap(), None);
        assert_eq!(tags.resolve("t").unwrap(), None);
        assert_eq!(tags.resolve("keep").unwrap().unwrap().id, id);
        assert!(!store.resolve_id(&id).unwrap().0.is_empty());

        match tags.delete("t").unwrap_err() {
            StoreError::TagNotFound(name) => assert_eq!(name, "t"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn targets_pair_each_tag_with_its_object_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let tags = TagStore::from(&store);
        let a = note(&store, "a");
        let b = note(&store, "b");
        let added_b = tags.add("beta", &b, false).unwrap();
        let added_a = tags.add("alpha", &a, false).unwrap();
        NoteStore::from(&store).delete(&b).unwrap();

        assert_eq!(
            tags.targets().unwrap(),
            vec![
                TagTarget {
                    name: "alpha".to_string(),
                    id: a,
                    commit_sha: added_a.commit_sha,
                },
                TagTarget {
                    name: "beta".to_string(),
                    id: b,
                    commit_sha: added_b.commit_sha,
                },
            ]
        );
    }

    #[test]
    fn records_are_in_name_order_and_flag_dangling_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let tags = TagStore::from(&store);
        assert_eq!(tags.records().unwrap(), Vec::new());

        let a = note(&store, "a");
        let b = note(&store, "b");
        tags.add("zeta", &a, false).unwrap();
        tags.add("alpha/one", &b, false).unwrap();
        tags.add("alpha/two", &a, false).unwrap();
        NoteStore::from(&store).delete(&b).unwrap();
        let (_, a_tip) = store.resolve_id(&a).unwrap();

        let listed = tags.records().unwrap();
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["alpha/one", "alpha/two", "zeta"]);
        assert_eq!(listed[0].id, b);
        assert!(listed[0].deleted);
        assert_eq!(listed[1].id, a);
        assert!(!listed[1].deleted);
        assert_eq!(listed[1].commit_sha, a_tip);
        assert_eq!(listed[1].object_type, "gage::note");
        assert!(listed[1].modified_ms.is_some());
    }

    #[test]
    fn a_tag_resolves_through_a_typed_store_and_is_type_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let id = note(&store, "v1");
        TagStore::from(&store).add("t", &id, false).unwrap();

        assert_eq!(NoteStore::from(&store).get("t").unwrap().id, id);
        match DatasetStore::from(&store).get("t").unwrap_err() {
            StoreError::WrongType {
                id: wrong,
                expected,
                actual,
            } => {
                assert_eq!(wrong, id);
                assert_eq!(expected, "gage::dataset");
                assert_eq!(actual, "gage::note");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn an_unknown_objectish_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        match store.resolve_id("nothing").unwrap_err() {
            StoreError::ObjectNotFound(v) => assert_eq!(v, "nothing"),
            other => panic!("unexpected error: {other}"),
        }
    }
}
