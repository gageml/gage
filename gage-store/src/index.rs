//! The object index: a rebuildable, persistent selection layer over the
//! object graph.
//!
//! The refs are the log. Git's state for the object graph is the map of
//! `refs/gage/object/<id>` to tip SHA, and one `for-each-ref` returns
//! it without reading any object. The index records the tip it last
//! saw for every ref, so validation is the diff between that table and
//! the live map; only refs whose tip changed are re-read, and re-reading
//! walks the new tip's `parent` chain and link parents until a commit
//! the index already holds. Any change that reaches the repository by
//! any path, including a store write, a fetch, or a direct
//! `git update-ref`, moves a tip and is caught by the same diff. The
//! store's own writes also go through [`Store::index_commit`] so a
//! process sees its writes without a second reconcile.
//!
//! What the index holds is defined by the object model, not by any
//! type: every commit's markers, every ref's tip, and every link. Type
//! modules opt attributes in by declaring JSON paths into `attrs.json`
//! (`INDEXED_ATTRS`); the reconcile extracts those and nothing else.
//!
//! The index is safe to delete at any time. A missing file or a
//! `schema_version` mismatch rebuilds it from an empty ref table.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::object::{LinkFile, Object};
use crate::{Store, StoreError, dataset, note, session};

/// Indexed attribute paths for an object type, or empty when the type
/// declares none.
pub(crate) fn indexed_attrs(object_type: &str) -> &'static [&'static str] {
    match object_type {
        note::OBJECT_TYPE => note::INDEXED_ATTRS,
        dataset::OBJECT_TYPE => dataset::INDEXED_ATTRS,
        session::OBJECT_TYPE => session::INDEXED_ATTRS,
        _ => &[],
    }
}

/// Extract the declared attribute values of `object` as
/// `(path, value)` pairs. Only scalar values are indexed; a missing
/// path or a non-scalar value yields nothing.
pub(crate) fn extract_attrs(object: &Object) -> Vec<(&'static str, String)> {
    let Some(attrs) = &object.tree.attrs else {
        return Vec::new();
    };
    indexed_attrs(&object.header.object_type)
        .iter()
        .filter_map(|path| {
            let pointer = format!("/{}", path.replace('.', "/"));
            let value = match attrs.pointer(&pointer)? {
                JsonValue::String(s) => s.clone(),
                JsonValue::Number(n) => n.to_string(),
                JsonValue::Bool(b) => b.to_string(),
                JsonValue::Null | JsonValue::Array(_) | JsonValue::Object(_) => return None,
            };
            Some((*path, value))
        })
        .collect()
}

/// Sort order for a query. Timestamps are the object's own `created`
/// and `modified` markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    CreatedAsc,
    #[default]
    CreatedDesc,
    ModifiedAsc,
    ModifiedDesc,
}

/// A selection over the current version of every object of one type:
/// an AND of equality tests on declared attribute paths, an order, and
/// a limit. Tombstones are never selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectQuery {
    pub(crate) object_type: &'static str,
    pub(crate) attrs: Vec<(&'static str, String)>,
    pub(crate) order: Order,
    pub(crate) limit: Option<usize>,
}

impl ObjectQuery {
    pub(crate) fn new(object_type: &'static str) -> Self {
        ObjectQuery {
            object_type,
            attrs: Vec::new(),
            order: Order::default(),
            limit: None,
        }
    }
}

/// What the store needs from an index. The write side is fed by the
/// reconcile and by the store's own writes; the read side serves
/// queries. One implementation exists, [`SqliteIndex`](crate::sqlite_index::SqliteIndex).
pub trait ObjectIndex {
    /// Ref id to tip SHA as last indexed. The reconcile diff runs
    /// against this.
    fn tips(&self) -> Result<BTreeMap<String, String>, StoreError>;

    /// True when this commit is already indexed; bounds the
    /// incremental walk.
    fn has_commit(&self, sha: &str) -> Result<bool, StoreError>;

    /// Record one object at one commit with its link files and its
    /// extracted attributes. Idempotent.
    fn put(
        &self,
        object: &Object,
        links: &[LinkFile],
        attrs: &[(&'static str, String)],
    ) -> Result<(), StoreError>;

    /// Record that a ref now points at `tip`, or was removed when
    /// `tip` is `None`.
    fn set_tip(&self, id: &str, tip: Option<&str>) -> Result<(), StoreError>;

    /// Tip SHAs selected by `query`, in query order.
    fn select(&self, query: &ObjectQuery) -> Result<Vec<String>, StoreError>;
}

impl Store {
    /// Bring the index up to date with the repository: diff the live
    /// ref map against the index's tips, index every commit reachable
    /// from a changed tip that the index does not hold, and drop refs
    /// that no longer exist.
    pub(crate) fn reconcile(&self) -> Result<(), StoreError> {
        let known = self.index.tips()?;
        let live: BTreeMap<String, String> = self
            .list_object_refs()?
            .into_iter()
            .map(|r| (r.id, r.tip_sha))
            .collect();
        for (id, tip) in &live {
            if known.get(id) != Some(tip) {
                self.index_from(tip)?;
                self.index.set_tip(id, Some(tip))?;
            }
        }
        for id in known.keys() {
            if !live.contains_key(id) {
                self.index.set_tip(id, None)?;
            }
        }
        Ok(())
    }

    /// Index `sha` and every commit reachable from it through `parent`
    /// and link files that the index does not already hold.
    fn index_from(&self, sha: &str) -> Result<(), StoreError> {
        let mut pending = vec![sha.to_string()];
        while let Some(sha) = pending.pop() {
            if self.index.has_commit(&sha)? {
                continue;
            }
            let (object, links) = self.index_commit(&sha)?;
            pending.extend(object.header.parent.iter().cloned());
            pending.extend(links.iter().flat_map(|l| l.shas.iter().cloned()));
        }
        Ok(())
    }

    /// Read the object at `sha` and record it in the index. Used by
    /// the reconcile walk.
    pub(crate) fn index_commit(&self, sha: &str) -> Result<(Object, Vec<LinkFile>), StoreError> {
        let object = self.read_object(sha)?;
        let links = self.find_link_files(sha)?;
        let attrs = extract_attrs(&object);
        self.index.put(&object, &links, &attrs)?;
        Ok((object, links))
    }

    /// Record an object the store just wrote and point its ref at it.
    /// Nothing is read back: the writer holds the header, the content,
    /// and the top-level link files. Link files inside an opaque
    /// subtree are found by walking it, which only happens when the
    /// object has one.
    pub(crate) fn index_written(&self, object: &Object) -> Result<(), StoreError> {
        let links = if object.tree.subtrees.is_empty() {
            object.top_level_links()
        } else {
            self.find_link_files(&object.commit_sha)?
        };
        let attrs = extract_attrs(object);
        self.index.put(object, &links, &attrs)?;
        self.index
            .set_tip(&object.header.id, Some(&object.commit_sha))
    }

    /// Tip SHAs selected by `query`, in query order.
    pub(crate) fn select(&self, query: &ObjectQuery) -> Result<Vec<String>, StoreError> {
        self.index.select(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{git_in, run};
    use crate::note::{NoteInput, NoteStore};
    use crate::object::object_ref;
    use crate::sqlite_index::INDEX_SCHEMA_VERSION;
    use crate::{DatasetStore, init};
    use serde_json::json;
    use std::path::Path;

    fn init_repo(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("store.git");
        init(&path).unwrap();
        path
    }

    fn note(store: &Store, name: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: "v",
                author: "user:test",
                targets: &[],
            })
            .unwrap()
    }

    fn note_ids(store: &Store) -> Vec<String> {
        NoteStore::from(store)
            .query()
            .order(Order::CreatedAsc)
            .iter()
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect()
    }

    #[test]
    fn write_through_makes_objects_selectable_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let a = note(&store, "a");
        let b = note(&store, "b");
        assert_eq!(note_ids(&store), vec![a, b]);
    }

    #[test]
    fn reopen_reconciles_objects_written_by_another_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let a = note(&Store::open(&path).unwrap(), "a");
        // Remove the index so the second open starts from nothing.
        std::fs::remove_file(tmp.path().join("cache/object-index.sqlite")).unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(note_ids(&store), vec![a]);
    }

    #[test]
    fn reconcile_catches_ref_moved_outside_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let id = note(&store, "a");
        let first = store.rev_parse(&object_ref(&id)).unwrap().unwrap();

        // Edit through a second handle, whose index is a separate
        // connection to the same file, then move the ref back with git
        // so the first handle's index is stale in both directions.
        let other = Store::open(&path).unwrap();
        NoteStore::from(&other).edit(&id, "v2").unwrap();
        run(git_in(&path, ["update-ref", &object_ref(&id), &first])).unwrap();

        let reopened = Store::open(&path).unwrap();
        let shas = reopened
            .select(&ObjectQuery::new(note::OBJECT_TYPE))
            .unwrap();
        assert_eq!(shas, vec![first]);
    }

    #[test]
    fn reconcile_drops_refs_deleted_outside_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let a = note(&store, "a");
        let b = note(&store, "b");
        run(git_in(&path, ["update-ref", "-d", &object_ref(&a)])).unwrap();

        let reopened = Store::open(&path).unwrap();
        assert_eq!(note_ids(&reopened), vec![b]);
        assert!(!reopened.index.tips().unwrap().contains_key(&a));
    }

    #[test]
    fn tombstones_are_not_selected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let a = note(&store, "a");
        let b = note(&store, "b");
        NoteStore::from(&store).delete(&a).unwrap();
        assert_eq!(note_ids(&store), vec![b]);
    }

    #[test]
    fn linked_commits_stay_indexed_after_their_ref_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let notes = NoteStore::from(&store);
        let root = note(&store, "root");
        let root_first = store.rev_parse(&object_ref(&root)).unwrap().unwrap();
        notes
            .create(NoteInput {
                name: "reply",
                value: "v",
                author: "user:test",
                targets: &[format!("note:{root}")],
            })
            .unwrap();
        notes.edit(&root, "v2").unwrap();

        std::fs::remove_file(tmp.path().join("cache/object-index.sqlite")).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.index.has_commit(&root_first).unwrap());
    }

    #[test]
    fn schema_version_mismatch_rebuilds_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let a = note(&Store::open(&path).unwrap(), "a");

        let index_path = tmp.path().join("cache/object-index.sqlite");
        let conn = rusqlite::Connection::open(&index_path).unwrap();
        conn.execute(
            "UPDATE meta SET schema_version = ?1",
            [INDEX_SCHEMA_VERSION + 1],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert_eq!(note_ids(&store), vec![a]);
        let conn = rusqlite::Connection::open(&index_path).unwrap();
        let version: u32 = conn
            .query_row("SELECT schema_version FROM meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn datasets_and_notes_select_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        note(&store, "a");
        let d = DatasetStore::from(&store).create().unwrap();
        let ids: Vec<String> = DatasetStore::from(&store)
            .iter()
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(ids, vec![d]);
    }

    fn object_with(object_type: &str, attrs: JsonValue) -> Object {
        Object::for_test(object_type, attrs)
    }

    #[test]
    fn extract_attrs_follows_declared_paths_only() {
        let object = object_with(
            session::OBJECT_TYPE,
            json!({ "driver": "claude 0.2.0", "summary": { "model": "claude-opus-5", "size": 12 } }),
        );
        assert_eq!(
            extract_attrs(&object),
            vec![("summary.model", "claude-opus-5".to_string())]
        );
    }

    #[test]
    fn extract_attrs_skips_missing_and_non_scalar_values() {
        let object = object_with(note::OBJECT_TYPE, json!({ "author": "user:x" }));
        assert!(extract_attrs(&object).is_empty());
        let object = object_with(note::OBJECT_TYPE, json!({ "name": ["a"] }));
        assert!(extract_attrs(&object).is_empty());
        let object = object_with("gage::unknown", json!({ "name": "n" }));
        assert!(extract_attrs(&object).is_empty());
    }
}
