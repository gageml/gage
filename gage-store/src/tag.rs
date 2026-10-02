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
        self.validate_name(name)?;
        let found = self.store.resolve_objectish(objectish)?;
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

    /// The object a tag names, or `None` when no tag has the name.
    /// The match is the object's current state, which may be a
    /// tombstone; the tag is then dangling.
    pub fn resolve(&self, name: &str) -> Result<Option<IdMatch>, StoreError> {
        let Some(sha) = self.store.rev_parse(&tag_ref(name))? else {
            return Ok(None);
        };
        let id = self.store.read_header(&sha)?.id;
        let tip = self
            .store
            .rev_parse(&object_ref(&id))?
            .ok_or_else(|| StoreError::ObjectNotFound(id.clone()))?;
        let header = self.store.read_header(&tip)?;
        let deleted = header.is_tombstone();
        Ok(Some(IdMatch {
            id,
            tip_sha: tip,
            object_type: header.object_type,
            deleted,
        }))
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

impl Store {
    /// Resolve an object-ish: a tag name, matched exactly, else an id
    /// or unique prefix as [`Store::resolve_id`] resolves it. A tag
    /// name that is also a valid prefix resolves as the tag.
    pub fn resolve_objectish(&self, value: &str) -> Result<IdMatch, StoreError> {
        if let Some(found) = TagStore::from(self).resolve(value)? {
            return Ok(found);
        }
        self.resolve_in(value, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;
    use crate::{NoteEdit, NoteInput, NoteStore, NoteValue};

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

        let found = store.resolve_objectish("t").unwrap();
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

        let found = store.resolve_objectish("t").unwrap();
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

        assert_eq!(store.resolve_objectish(&prefix).unwrap().id, b);
        assert_eq!(store.resolve_id(&prefix).unwrap().0, a);
    }

    #[test]
    fn an_unknown_objectish_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());

        match store.resolve_objectish("nothing").unwrap_err() {
            StoreError::ObjectNotFound(v) => assert_eq!(v, "nothing"),
            other => panic!("unexpected error: {other}"),
        }
    }
}
