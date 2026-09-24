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
//! store's own writes are recorded by [`Store::record_write`] as they
//! happen, so a process sees its writes without a second reconcile.
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

use crate::git::{git_in, run};
use crate::object::{LinkFile, Object, object_ref};
use crate::{Store, StoreError, dataset, note, scan, session};

/// Indexed attribute paths for an object type, or empty when the type
/// declares none.
pub(crate) fn indexed_attrs(object_type: &str) -> &'static [&'static str] {
    match object_type {
        note::OBJECT_TYPE => note::INDEXED_ATTRS,
        dataset::OBJECT_TYPE => dataset::INDEXED_ATTRS,
        session::OBJECT_TYPE => session::INDEXED_ATTRS,
        scan::OBJECT_TYPE => scan::INDEXED_ATTRS,
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

/// One tip a selection matched: the object's id, the commit at its
/// ref, and the commit's timestamps. Everything here comes from the
/// index; no object is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedTip {
    pub id: String,
    pub sha: String,
    pub created_ms: Option<i64>,
    pub modified_ms: Option<i64>,
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

/// One ref matched by an id prefix, with what a caller needs to
/// present or reject it without reading the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMatch {
    pub id: String,
    pub tip_sha: String,
    /// The object type at the tip, e.g. `gage::note`.
    pub object_type: String,
    /// True when the tip is a tombstone.
    pub deleted: bool,
}

/// What the store needs from an index. The write side is fed by the
/// reconcile and by the store's own writes; the read side serves
/// queries. One implementation exists, [`SqliteIndex`](crate::sqlite_index::SqliteIndex).
pub trait ObjectIndex: Send {
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

    /// Drop every commit not reachable from a tip through `parent` and
    /// link edges, with its links and attributes. Returns the number
    /// of commits dropped. Runs inside the caller's transaction.
    fn prune(&self) -> Result<usize, StoreError>;

    /// Tips selected by `query`, in query order.
    fn select(&self, query: &ObjectQuery) -> Result<Vec<SelectedTip>, StoreError>;

    /// Ids of the most recently modified live tips, newest first,
    /// restricted to `object_type` when given.
    fn recent_ids(
        &self,
        object_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, StoreError>;

    /// Every ref whose id starts with `prefix`, live or tombstoned, in
    /// id order.
    fn ids_with_prefix(&self, prefix: &str) -> Result<Vec<IdMatch>, StoreError>;

    /// Start a transaction. Every `put` and `set_tip` until `commit`
    /// or `rollback` is applied as one unit.
    fn begin(&self) -> Result<(), StoreError>;
    fn commit(&self) -> Result<(), StoreError>;
    fn rollback(&self) -> Result<(), StoreError>;
}

impl Store {
    /// Bring the index up to date with the repository: diff the live
    /// ref map against the index's tips, index every commit reachable
    /// from a changed tip that the index does not hold, and drop refs
    /// that no longer exist.
    ///
    /// The whole diff is one index transaction. With one commit per
    /// object, each a WAL sync, a rebuild of the bench population took
    /// 11.6 s; as one transaction it takes 251 ms, a factor of 46. See
    /// footnote 2 of `gage-bench/results/store/README.md`.
    pub(crate) fn reconcile(&self) -> Result<(), StoreError> {
        // Both reads happen inside the transaction so the diff and the
        // writes see one state.
        self.in_index_transaction(|| {
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
        })
    }

    /// Run `f` inside one index transaction, rolling back on error.
    /// The rollback outcome is not reported: the original error is the
    /// one the caller needs, and a failed rollback leaves the
    /// transaction to be discarded when the connection closes.
    pub(crate) fn in_index_transaction<T>(
        &self,
        f: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.index.begin()?;
        match f() {
            Ok(value) => {
                self.index.commit()?;
                Ok(value)
            }
            Err(e) => {
                drop(self.index.rollback());
                Err(e)
            }
        }
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

    /// Record a write the store just made: index the object with the
    /// link files the writer found, then point its ref at it, inside
    /// one index transaction. The ref moves last, so a failed index
    /// write leaves the ref where it was and `Err` means the object is
    /// not published. `previous_tip` is the ref value the update
    /// requires; empty means the ref must not exist yet.
    ///
    /// Nothing is read back from the repository. That holds only while
    /// what this records equals what [`Store::index_commit`] records
    /// for the same commit on a rebuild, row for row;
    /// `rebuild_matches_write_through_after_gc` pins it.
    pub(crate) fn record_write(
        &self,
        object: &Object,
        links: &[LinkFile],
        previous_tip: &str,
    ) -> Result<(), StoreError> {
        self.wrote.set(true);
        let attrs = extract_attrs(object);
        self.in_index_transaction(|| {
            self.index.put(object, links, &attrs)?;
            self.index
                .set_tip(&object.header.id, Some(&object.commit_sha))?;
            run(git_in(
                self.path(),
                [
                    "update-ref",
                    &object_ref(&object.header.id),
                    &object.commit_sha,
                    previous_tip,
                ],
            ))?;
            Ok(())
        })
    }

    /// Tips selected by `query`, in query order.
    pub(crate) fn select(&self, query: &ObjectQuery) -> Result<Vec<SelectedTip>, StoreError> {
        self.index.select(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DatasetStore;
    use crate::note::{NoteEdit, NoteInput, NoteStore, NoteValue};
    use crate::object::ObjectTree;
    use crate::session::SessionStore;
    use crate::sqlite_index::INDEX_SCHEMA_VERSION;
    use crate::test_support::{FsckGuard, fsck_guard, init_for_test, open_store};
    use crate::writer::{TreeInput, mktree, write_blob};
    use gage_session::{
        ContentSink, ContentSource, Driver, DriverError, NativeSession, SessionAttrs, Source,
        StoredSession,
    };
    use serde_json::json;
    use std::any::Any;
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    fn init_repo(dir: &Path) -> (std::path::PathBuf, FsckGuard) {
        let path = dir.join("store.git");
        init_for_test(&path);
        let guard = fsck_guard(&path);
        (path, guard)
    }

    fn note(store: &Store, name: &str) -> String {
        NoteStore::from(store)
            .create(NoteInput {
                name,
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: None,
                metadata: None,
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
        let (path, _fsck) = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let a = note(&store, "a");
        let b = note(&store, "b");
        assert_eq!(note_ids(&store), vec![a, b]);
    }

    #[test]
    fn reopen_reconciles_objects_written_by_another_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
        let a = note(&Store::open(&path).unwrap(), "a");
        // Remove the index so the second open starts from nothing.
        std::fs::remove_file(tmp.path().join("cache/object-index.sqlite")).unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(note_ids(&store), vec![a]);
    }

    /// Delegates to the real index but refuses every `put`, to stand in
    /// for an index failure after the git objects are written.
    struct FailingPut {
        inner: Option<Box<dyn ObjectIndex>>,
    }

    impl FailingPut {
        fn inner(&self) -> &dyn ObjectIndex {
            self.inner
                .as_deref()
                .expect("wrapper is installed with an inner index")
        }
    }

    impl ObjectIndex for FailingPut {
        fn tips(&self) -> Result<BTreeMap<String, String>, StoreError> {
            self.inner().tips()
        }
        fn has_commit(&self, sha: &str) -> Result<bool, StoreError> {
            self.inner().has_commit(sha)
        }
        fn put(
            &self,
            _object: &Object,
            _links: &[LinkFile],
            _attrs: &[(&'static str, String)],
        ) -> Result<(), StoreError> {
            Err(StoreError::Index("injected put failure".to_string()))
        }
        fn set_tip(&self, id: &str, tip: Option<&str>) -> Result<(), StoreError> {
            self.inner().set_tip(id, tip)
        }
        fn prune(&self) -> Result<usize, StoreError> {
            self.inner().prune()
        }
        fn select(&self, query: &ObjectQuery) -> Result<Vec<SelectedTip>, StoreError> {
            self.inner().select(query)
        }
        fn recent_ids(
            &self,
            object_type: Option<&str>,
            limit: usize,
        ) -> Result<Vec<String>, StoreError> {
            self.inner().recent_ids(object_type, limit)
        }
        fn ids_with_prefix(&self, prefix: &str) -> Result<Vec<IdMatch>, StoreError> {
            self.inner().ids_with_prefix(prefix)
        }
        fn begin(&self) -> Result<(), StoreError> {
            self.inner().begin()
        }
        fn commit(&self) -> Result<(), StoreError> {
            self.inner().commit()
        }
        fn rollback(&self) -> Result<(), StoreError> {
            self.inner().rollback()
        }
    }

    /// Delegates to the real index and records the link files every
    /// `put` receives.
    struct CapturingPut {
        inner: Option<Box<dyn ObjectIndex>>,
        seen: Arc<Mutex<Vec<LinkFile>>>,
    }

    impl CapturingPut {
        fn inner(&self) -> &dyn ObjectIndex {
            self.inner
                .as_deref()
                .expect("wrapper is installed with an inner index")
        }
    }

    impl ObjectIndex for CapturingPut {
        fn tips(&self) -> Result<BTreeMap<String, String>, StoreError> {
            self.inner().tips()
        }
        fn has_commit(&self, sha: &str) -> Result<bool, StoreError> {
            self.inner().has_commit(sha)
        }
        fn put(
            &self,
            object: &Object,
            links: &[LinkFile],
            attrs: &[(&'static str, String)],
        ) -> Result<(), StoreError> {
            self.seen.lock().unwrap().extend(links.iter().cloned());
            self.inner().put(object, links, attrs)
        }
        fn set_tip(&self, id: &str, tip: Option<&str>) -> Result<(), StoreError> {
            self.inner().set_tip(id, tip)
        }
        fn prune(&self) -> Result<usize, StoreError> {
            self.inner().prune()
        }
        fn select(&self, query: &ObjectQuery) -> Result<Vec<SelectedTip>, StoreError> {
            self.inner().select(query)
        }
        fn recent_ids(
            &self,
            object_type: Option<&str>,
            limit: usize,
        ) -> Result<Vec<String>, StoreError> {
            self.inner().recent_ids(object_type, limit)
        }
        fn ids_with_prefix(&self, prefix: &str) -> Result<Vec<IdMatch>, StoreError> {
            self.inner().ids_with_prefix(prefix)
        }
        fn begin(&self) -> Result<(), StoreError> {
            self.inner().begin()
        }
        fn commit(&self) -> Result<(), StoreError> {
            self.inner().commit()
        }
        fn rollback(&self) -> Result<(), StoreError> {
            self.inner().rollback()
        }
    }

    #[test]
    fn write_indexes_links_inside_schema_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _fsck) = open_store(tmp.path());
        let target = note(&store, "target");
        let target_sha = store.rev_parse(&object_ref(&target)).unwrap().unwrap();

        // tasks/x/agent_sessions.link, a link file two levels down in a
        // schema subtree, beside an opaque logs.d holding a decoy
        let link_blob = write_blob(store.path(), format!("{target_sha}\n").as_bytes()).unwrap();
        let decoy_blob = write_blob(store.path(), b"not a sha\n").unwrap();
        let logs = mktree(
            store.path(),
            &[TreeInput {
                mode: "100644",
                sha: &decoy_blob,
                name: "decoy.link",
            }],
        )
        .unwrap();
        let task = mktree(
            store.path(),
            &[
                TreeInput {
                    mode: "100644",
                    sha: &link_blob,
                    name: "agent_sessions.link",
                },
                TreeInput {
                    mode: "040000",
                    sha: &logs,
                    name: "logs.d",
                },
            ],
        )
        .unwrap();
        let tasks = mktree(
            store.path(),
            &[TreeInput {
                mode: "040000",
                sha: &task,
                name: "x",
            }],
        )
        .unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let inner = std::mem::replace(
            &mut store.index,
            Box::new(CapturingPut {
                inner: None,
                seen: Arc::clone(&seen),
            }),
        );
        store.index = Box::new(CapturingPut {
            inner: Some(inner),
            seen: Arc::clone(&seen),
        });

        let mut tree = ObjectTree::default();
        tree.subtrees.insert("tasks".to_string(), tasks);
        let commit = store
            .create("gage::test", "1", "scan", &tree, "test")
            .unwrap();

        let expected = vec![LinkFile {
            path: "tasks/x/agent_sessions.link".to_string(),
            shas: vec![target_sha],
        }];
        assert_eq!(*seen.lock().unwrap(), expected, "links indexed on write");
        assert_eq!(
            store.find_link_files(&commit).unwrap(),
            expected,
            "links found on rebuild"
        );
    }

    #[test]
    fn failed_index_write_leaves_the_ref_unmoved() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut store, _fsck) = open_store(tmp.path());
        let id = note(&store, "a");
        let before = store.rev_parse(&object_ref(&id)).unwrap().unwrap();

        let inner = std::mem::replace(&mut store.index, Box::new(FailingPut { inner: None }));
        store.index = Box::new(FailingPut { inner: Some(inner) });

        let err = NoteStore::from(&store)
            .edit(
                &id,
                NoteEdit {
                    value: Some(NoteValue::Text("v2".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::Index(_)), "{err}");
        assert_eq!(store.rev_parse(&object_ref(&id)).unwrap().unwrap(), before);

        let reopened = Store::open(store.path()).unwrap();
        assert_eq!(
            reopened.rev_parse(&object_ref(&id)).unwrap().unwrap(),
            before
        );
        assert_eq!(
            NoteStore::from(&reopened).get(&id).unwrap().value,
            NoteValue::Text("v".into())
        );
    }

    /// A session with one file under the opaque `files.d` subtree.
    struct FakeSession {
        id: String,
        source: String,
        attrs: FakeAttrs,
    }

    struct FakeAttrs;

    impl SessionAttrs for FakeAttrs {
        fn native_mtime(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }
        fn native_size(&self) -> u64 {
            2
        }
        fn is_empty(&self) -> bool {
            false
        }
        fn project_name(&self) -> Option<&str> {
            None
        }
        fn title(&self) -> Option<&str> {
            None
        }
        fn model(&self) -> Option<&str> {
            None
        }
        fn message_count(&self) -> Option<u64> {
            None
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
            &self.attrs
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct FakeDriver;

    impl Driver for FakeDriver {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn version(&self) -> &'static str {
            "0.1"
        }
        fn schemes(&self) -> &'static [&'static str] {
            &["fake"]
        }
        fn open_source(&self, _source: &str) -> Result<Box<dyn Source>, DriverError> {
            Err(DriverError::Other("open_source not used".into()))
        }
        fn write_native(
            &self,
            _session: &mut dyn NativeSession,
            sink: &mut dyn ContentSink,
        ) -> Result<String, DriverError> {
            let mut w = sink.create("session.jsonl").map_err(DriverError::Io)?;
            w.write_all(b"{}").map_err(DriverError::Io)?;
            Ok("fake-lines 1".to_string())
        }
        fn read_stored(
            &self,
            _native_id: String,
            _content_format: &str,
            _source: Box<dyn ContentSource>,
        ) -> Result<Box<dyn StoredSession>, DriverError> {
            Err(DriverError::Other("read_stored not used".into()))
        }
    }

    /// Every row of every index table except `meta`, sorted.
    fn dump_index(path: &Path) -> Vec<String> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mut rows = Vec::new();
        for table in ["ref", "object", "link", "attr"] {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = stmt.column_count();
            let mut query = stmt.query([]).unwrap();
            while let Some(row) = query.next().unwrap() {
                let cells: Vec<String> = (0..columns)
                    .map(|i| format!("{:?}", row.get::<_, rusqlite::types::Value>(i).unwrap()))
                    .collect();
                rows.push(format!("{table}: {}", cells.join(" | ")));
            }
        }
        rows.sort();
        rows
    }

    /// `record_write` indexes what the writer holds without reading
    /// back, and a delete leaves the old version's rows for `gc` to
    /// prune. This pins that after `gc` the index equals a rebuild
    /// from the refs, row for row, for every kind of write.
    #[test]
    fn rebuild_matches_write_through_after_gc() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
        let index_path = tmp.path().join(crate::store::INDEX_FILE);
        let store = Store::open(&path).unwrap();
        let notes = NoteStore::from(&store);
        let tip = |id: &str| store.rev_parse(&object_ref(id)).unwrap().unwrap();

        // Notes: a target, a link to it, and an edit
        let a = note(&store, "a");
        notes
            .create(NoteInput {
                name: "b",
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: Some(&format!("note:{a}")),
                metadata: None,
            })
            .unwrap();
        notes
            .edit(
                &a,
                NoteEdit {
                    value: Some(NoteValue::Text("v2".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();

        // Two deletes: c1's first version stays reachable through d's
        // link; c2's first version becomes unreachable
        let c1 = note(&store, "c1");
        let c1_first = tip(&c1);
        notes
            .create(NoteInput {
                name: "d",
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: Some(&format!("note:{c1}")),
                metadata: None,
            })
            .unwrap();
        let c2 = note(&store, "c2");
        let c2_first = tip(&c2);
        notes.delete(&c1).unwrap();
        notes.delete(&c2).unwrap();

        // A session, whose only subtree is opaque
        SessionStore::from(&store)
            .add(
                &FakeDriver,
                &mut FakeSession {
                    id: "s1".into(),
                    source: "fake:s1".into(),
                    attrs: FakeAttrs,
                },
            )
            .unwrap();

        // An object with a link file inside a schema subtree
        let a_sha = tip(&a);
        let link_blob = write_blob(&path, format!("{a_sha}\n").as_bytes()).unwrap();
        let task = mktree(
            &path,
            &[TreeInput {
                mode: "100644",
                sha: &link_blob,
                name: "agent_sessions.link",
            }],
        )
        .unwrap();
        let mut tree = ObjectTree::default();
        tree.subtrees.insert("tasks".to_string(), task);
        store
            .create("gage::test", "1", "scan", &tree, "scan")
            .unwrap();

        let before_gc = dump_index(&index_path);
        store.gc(Some("now"), true).unwrap();
        let after_gc = dump_index(&index_path);
        let pruned: Vec<&String> = before_gc.iter().filter(|r| !after_gc.contains(r)).collect();
        assert!(!pruned.is_empty(), "delete leaves rows for gc");
        assert!(pruned.iter().all(|r| r.contains(&c2_first)), "{pruned:?}");
        assert!(
            after_gc.iter().any(|r| r.contains(&c1_first)),
            "linked version survives"
        );
        assert!(
            after_gc.iter().any(|r| r.starts_with("link:")),
            "{after_gc:?}"
        );
        assert!(
            after_gc.iter().any(|r| r.starts_with("attr:")),
            "{after_gc:?}"
        );
        drop(store);

        for suffix in ["", "-wal", "-shm"] {
            let file = index_path.with_file_name(format!(
                "{}{suffix}",
                index_path.file_name().unwrap().to_str().unwrap()
            ));
            if file.exists() {
                std::fs::remove_file(&file).unwrap();
            }
        }
        let rebuilt_store = Store::open(&path).unwrap();
        let rebuilt = dump_index(&index_path);
        drop(rebuilt_store);
        assert_eq!(after_gc, rebuilt);
    }

    #[test]
    fn delete_prunes_the_index_at_close() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
        let index_path = tmp.path().join(crate::store::INDEX_FILE);
        let store = Store::open(&path).unwrap();
        let keep = note(&store, "keep");
        let gone = note(&store, "gone");
        let gone_first = store.rev_parse(&object_ref(&gone)).unwrap().unwrap();
        NoteStore::from(&store).delete(&gone).unwrap();
        assert!(store.deleted.get());
        let before = dump_index(&index_path);
        assert!(before.iter().any(|r| r.contains(&gone_first)), "{before:?}");
        drop(store);

        let after = dump_index(&index_path);
        assert!(!after.iter().any(|r| r.contains(&gone_first)), "{after:?}");
        assert!(after.iter().any(|r| r.contains(&keep)), "{after:?}");
    }

    #[test]
    fn reconcile_catches_ref_moved_outside_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let id = note(&store, "a");
        let first = store.rev_parse(&object_ref(&id)).unwrap().unwrap();

        // Edit through a second handle, whose index is a separate
        // connection to the same file, then move the ref back with git
        // so the first handle's index is stale in both directions.
        let other = Store::open(&path).unwrap();
        NoteStore::from(&other)
            .edit(
                &id,
                NoteEdit {
                    value: Some(NoteValue::Text("v2".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();
        run(git_in(&path, ["update-ref", &object_ref(&id), &first])).unwrap();

        let reopened = Store::open(&path).unwrap();
        let shas: Vec<String> = reopened
            .select(&ObjectQuery::new(note::OBJECT_TYPE))
            .unwrap()
            .into_iter()
            .map(|t| t.sha)
            .collect();
        assert_eq!(shas, vec![first]);
    }

    #[test]
    fn reconcile_drops_refs_deleted_outside_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
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
        let (path, _fsck) = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let a = note(&store, "a");
        let b = note(&store, "b");
        NoteStore::from(&store).delete(&a).unwrap();
        assert_eq!(note_ids(&store), vec![b]);
    }

    #[test]
    fn linked_commits_stay_indexed_after_their_ref_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
        let store = Store::open(&path).unwrap();
        let notes = NoteStore::from(&store);
        let root = note(&store, "root");
        let root_first = store.rev_parse(&object_ref(&root)).unwrap().unwrap();
        notes
            .create(NoteInput {
                name: "reply",
                value: NoteValue::Text("v".into()),
                author: "user:test",
                target: Some(&format!("note:{root}")),
                metadata: None,
            })
            .unwrap();
        notes
            .edit(
                &root,
                NoteEdit {
                    value: Some(NoteValue::Text("v2".into())),
                    ..NoteEdit::default()
                },
            )
            .unwrap();

        std::fs::remove_file(tmp.path().join("cache/object-index.sqlite")).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.index.has_commit(&root_first).unwrap());
    }

    #[test]
    fn schema_version_mismatch_rebuilds_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, _fsck) = init_repo(tmp.path());
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
        let (path, _fsck) = init_repo(tmp.path());
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
