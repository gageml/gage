//! Keys: the identity an issue or an attachment carries from creation
//! in `attrs.key`, projected to the ref `refs/gage/1/key/<type>/<key>`
//! so that at most one live object carries it. A key is therefore a
//! ref name component sequence and follows git's ref rules, with one
//! addition: it is lowercase, so two keys cannot differ only by case
//! on a case-insensitive filesystem.

use crate::git::{git_in, run};
use crate::object::{Object, ObjectTree, object_ref};
use crate::refs::{KEY_REFS, key_ref};
use crate::{Store, StoreError};

/// Check `key` against the ref name rules. The checks restate what
/// `git check-ref-format` enforces for a component sequence, so no
/// git process is needed, plus the lowercase rule.
pub fn validate_key(key: &str) -> Result<(), StoreError> {
    let bad = || Err(StoreError::KeyName(key.to_string()));
    if key.is_empty() || key.starts_with('-') || key.ends_with('/') || key.ends_with('.') {
        return bad();
    }
    if key.contains("..") || key.contains("@{") || key.contains("//") {
        return bad();
    }
    if key
        .chars()
        .any(|c| c.is_ascii_uppercase() || c.is_ascii_control() || " ~^:?*[\\\x7f".contains(c))
    {
        return bad();
    }
    if key
        .split('/')
        .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return bad();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;
    use serde_json::json;

    const ISSUE: &str = crate::issue::OBJECT_TYPE;

    fn keyed_tree(key: &str, title: &str) -> ObjectTree {
        ObjectTree {
            attrs: Some(json!({ "key": key, "title": title })),
            ..ObjectTree::default()
        }
    }

    fn ref_sha(store: &Store, key: &str) -> Option<String> {
        store.rev_parse(&key_ref("issue", key)).unwrap()
    }

    #[test]
    fn create_edit_and_delete_move_the_key_ref_with_the_object() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let first = store
            .create(
                ISSUE,
                "1",
                "ID000000000000000000000001",
                &keyed_tree("k/one", "a"),
                "t",
            )
            .unwrap();
        assert_eq!(ref_sha(&store, "k/one").as_deref(), Some(first.as_str()));
        let live = resolve_key(&store, ISSUE, "k/one").unwrap().unwrap();
        assert_eq!(live.header.id, "ID000000000000000000000001");

        let current = store.read_object(&first).unwrap();
        let edited = match store
            .edit(&current, &keyed_tree("k/one", "b"), "t")
            .unwrap()
        {
            crate::object::EditOutcome::Written(sha) => sha,
            other => panic!("{other:?}"),
        };
        assert_eq!(ref_sha(&store, "k/one").as_deref(), Some(edited.as_str()));

        let current = store.read_object(&edited).unwrap();
        let tomb = store.delete(&current, "t").unwrap();
        assert_eq!(ref_sha(&store, "k/one").as_deref(), Some(tomb.as_str()));
        assert!(resolve_key(&store, ISSUE, "k/one").unwrap().is_none());
    }

    #[test]
    fn a_tombstoned_key_can_be_claimed_and_a_live_one_cannot() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let first = store
            .create(
                ISSUE,
                "1",
                "ID000000000000000000000001",
                &keyed_tree("k", "a"),
                "t",
            )
            .unwrap();
        let taken = store
            .create(
                ISSUE,
                "1",
                "ID000000000000000000000002",
                &keyed_tree("k", "b"),
                "t",
            )
            .unwrap_err();
        assert!(
            matches!(&taken, StoreError::KeyTaken { key, id } if key == "k" && id == "ID000000000000000000000001"),
            "{taken}"
        );
        assert!(ref_sha(&store, "k").is_some());

        let current = store.read_object(&first).unwrap();
        store.delete(&current, "t").unwrap();
        let second = store
            .create(
                ISSUE,
                "1",
                "ID000000000000000000000002",
                &keyed_tree("k", "b"),
                "t",
            )
            .unwrap();
        assert_eq!(ref_sha(&store, "k").as_deref(), Some(second.as_str()));
        let live = resolve_key(&store, ISSUE, "k").unwrap().unwrap();
        assert_eq!(live.header.id, "ID000000000000000000000002");
    }

    #[test]
    fn an_edit_cannot_change_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let first = store
            .create(
                ISSUE,
                "1",
                "ID000000000000000000000001",
                &keyed_tree("k", "a"),
                "t",
            )
            .unwrap();
        let current = store.read_object(&first).unwrap();
        let err = store
            .edit(&current, &keyed_tree("other", "a"), "t")
            .unwrap_err();
        assert!(matches!(err, StoreError::Parse(_)), "{err}");
    }

    #[test]
    fn rebuild_recreates_key_refs_from_the_objects() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(dir.path());
        let first = store
            .create(
                ISSUE,
                "1",
                "ID000000000000000000000001",
                &keyed_tree("k", "a"),
                "t",
            )
            .unwrap();
        run(git_in(
            store.path(),
            ["update-ref", "-d", &key_ref("issue", "k")],
        ))
        .unwrap();
        assert!(ref_sha(&store, "k").is_none());
        assert_eq!(store.rebuild_key_refs().unwrap(), 1);
        assert_eq!(ref_sha(&store, "k").as_deref(), Some(first.as_str()));
        let listed = KeyStore::from(&store).list().unwrap();
        assert_eq!(
            listed,
            [KeyRef {
                type_name: "issue".into(),
                key: "k".into(),
                id: "ID000000000000000000000001".into(),
                commit_sha: first,
            }]
        );
    }

    #[test]
    fn accepts_ref_safe_lowercase_keys() {
        for key in [
            "session-retention",
            "claude-config/5f3a9c1e2b7d4406",
            "a/b.c_d",
        ] {
            assert!(validate_key(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn rejects_keys_git_or_the_lowercase_rule_refuse() {
        for key in [
            "", "-x", "x/", "x.", "a..b", "a@{1}", "a//b", "Upper", "a b", "a~b", "a^b", "a?b",
            "a*b", "a[b", "a\\b", "/x", "a/.b", "a.lock", "a.lock/b",
        ] {
            assert!(
                matches!(validate_key(key), Err(StoreError::KeyName(k)) if k == key),
                "{key}"
            );
        }
    }
}

/// The key an object of `object_type` carries in `tree`, when the
/// type has keys: `attrs.key` on an issue or an attachment.
pub(crate) fn key_of(object_type: &str, tree: &ObjectTree) -> Option<String> {
    if object_type != crate::issue::OBJECT_TYPE && object_type != crate::attachment::OBJECT_TYPE {
        return None;
    }
    tree.attrs.as_ref()?.get("key")?.as_str().map(String::from)
}

/// The bare type name a key ref is filed under: `issue` for
/// `gage::issue`.
fn type_name(object_type: &str) -> &str {
    object_type.strip_prefix("gage::").unwrap_or(object_type)
}

/// A key ref write to run beside an object ref write: the ref and the
/// value it must hold for the update to apply (empty when it must not
/// exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyRefUpdate {
    pub ref_name: String,
    pub old: String,
}

/// The key ref write a new commit of object `id` of `object_type`
/// needs, if any. `prev` is the commit being superseded, `None` for a
/// create or a resurrection; `next` is the new content, `None` for a
/// tombstone. A key is intrinsic: an edit that changes it is refused.
/// A create under a key whose ref names another live object is
/// [`StoreError::KeyTaken`]; a ref left on a tombstone, or on this
/// object, is retargeted.
pub(crate) fn plan_key_ref(
    store: &Store,
    prev: Option<&Object>,
    object_type: &str,
    id: &str,
    next: Option<&ObjectTree>,
) -> Result<Option<KeyRefUpdate>, StoreError> {
    let prev_key = prev.and_then(|o| key_of(object_type, &o.tree));
    let next_key = next.and_then(|t| key_of(object_type, t));
    let key = match (&prev_key, &next_key) {
        (None, None) => return Ok(None),
        (Some(a), Some(b)) if a != b => {
            return Err(StoreError::Parse(format!(
                "object {id}: key is immutable, {a:?} cannot become {b:?}"
            )));
        }
        (Some(k), _) | (None, Some(k)) => k,
    };
    let ref_name = key_ref(type_name(object_type), key);
    let existing = store.rev_parse(&ref_name)?;
    if prev_key.is_none()
        && let Some(sha) = &existing
    {
        let header = store.read_header(sha)?;
        if !header.is_tombstone() && header.id != id {
            return Err(StoreError::KeyTaken {
                key: key.clone(),
                id: header.id,
            });
        }
    }
    Ok(Some(KeyRefUpdate {
        ref_name,
        old: existing.unwrap_or_default(),
    }))
}

/// The live object of `object_type` carrying `key`, read through the
/// key ref to the object's current commit, or `None` when no ref
/// exists or the object is a tombstone.
pub(crate) fn resolve_key(
    store: &Store,
    object_type: &str,
    key: &str,
) -> Result<Option<Object>, StoreError> {
    let Some(sha) = store.rev_parse(&key_ref(type_name(object_type), key))? else {
        return Ok(None);
    };
    let id = store.read_header(&sha)?.id;
    let Some(tip) = store.rev_parse(&object_ref(&id))? else {
        return Ok(None);
    };
    let object = store.read_object(&tip)?;
    if object.header.is_tombstone() {
        return Ok(None);
    }
    Ok(Some(object))
}

/// One key ref with the id of the object it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRef {
    /// The bare type name: `issue`, `attachment`
    pub type_name: String,
    pub key: String,
    /// The named object's id
    pub id: String,
    /// The commit the key ref points at
    pub commit_sha: String,
}

/// Typed view over a [`Store`] for key refs.
pub struct KeyStore<'a> {
    store: &'a Store,
}

impl<'a> From<&'a Store> for KeyStore<'a> {
    fn from(store: &'a Store) -> Self {
        KeyStore { store }
    }
}

impl KeyStore<'_> {
    /// Every key ref in ref order with the id of the object it names,
    /// one `id` blob read per ref.
    pub fn list(&self) -> Result<Vec<KeyRef>, StoreError> {
        let out = run(git_in(
            self.store.path(),
            [
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                KEY_REFS,
            ],
        ))?;
        let mut refs = Vec::new();
        for line in out.lines() {
            let (ref_name, sha) = line
                .split_once(' ')
                .ok_or_else(|| StoreError::Parse(format!("for-each-ref line: {line}")))?;
            let rest = ref_name.strip_prefix(KEY_REFS).ok_or_else(|| {
                StoreError::Parse(format!("key ref outside namespace: {ref_name}"))
            })?;
            let (type_name, key) = rest
                .split_once('/')
                .ok_or_else(|| StoreError::Parse(format!("key ref without a type: {ref_name}")))?;
            let bytes = self.store.read_blob_bytes(&format!("{sha}:id"))?;
            refs.push(KeyRef {
                type_name: type_name.to_string(),
                key: key.to_string(),
                id: String::from_utf8_lossy(&bytes).trim().to_string(),
                commit_sha: sha.to_string(),
            });
        }
        Ok(refs)
    }
}

impl Store {
    /// Recreate every key ref from the objects: for each object ref
    /// tip carrying `attrs.key`, point the key ref at it. Key refs are
    /// derived state, so this is the whole repair after a rebuild or
    /// a ref lost outside the store.
    pub fn rebuild_key_refs(&self) -> Result<usize, StoreError> {
        let mut written = 0;
        for object_ref in self.list_object_refs()? {
            let object = self.read_object(&object_ref.tip_sha)?;
            let Some(key) = key_of(&object.header.object_type, &object.tree) else {
                continue;
            };
            run(git_in(
                self.path(),
                [
                    "update-ref",
                    &key_ref(type_name(&object.header.object_type), &key),
                    &object.commit_sha,
                ],
            ))?;
            written += 1;
        }
        Ok(written)
    }
}
