//! Pending session publish operations.
//!
//! A publish is staged as a directory under Gage home,
//! `publish/<session id>/`, named by the new session's object id.
//! The directory holds the derived session's final tree layout:
//! `attrs.json` with `origin`, the `files.d` content, and
//! `redacted.json`. It is the same shape an added session's tree has,
//! so one decoder serves both the staged directory and a committed
//! session, the way a scan directory mirrors a stored scan. Beside it,
//! `publish/<session id>.publish.json` is the publish definition: the
//! instructions the staged session is derived from, which may hold
//! anything and is never committed. The session enters the store only
//! on apply; abort deletes the directory and the definition and
//! writes nothing.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use gage_core::uuid::new_uuid;

use crate::index::IdMatch;
use crate::session::{FILES_TREE, OBJECT_TYPE, SessionAttrsRecord};
use crate::{SessionRecord, SessionStore, Store, StoreError};

const PUBLISH_DIR: &str = "publish";
const ATTRS_FILE: &str = "attrs.json";
const REDACTED_FILE: &str = "redacted.json";
const DEFINITION_SUFFIX: &str = ".publish.json";

/// Whether the data layer includes pending publish working trees in
/// the session tables, set by `GAGE_SHOW_PUBLISHING=1`. Only the
/// exact value `1` turns it on.
pub fn show_publishing() -> bool {
    std::env::var_os("GAGE_SHOW_PUBLISHING").as_deref() == Some(OsStr::new("1"))
}

/// One pending publish working tree, read from its staging directory.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPublish {
    /// The new session's object id, the directory name.
    pub id: String,
    pub attrs: SessionAttrsRecord,
    pub root: PathBuf,
    /// UNIX time millis of the working tree's last change.
    pub modified_ms: i64,
}

/// The parent of publish staging directories, `publish/` under Gage
/// home.
pub fn publish_root() -> PathBuf {
    gage_core::config::gage_home().join(PUBLISH_DIR)
}

/// The on-disk layout of one pending publish, `publish/<id>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDirLayout {
    root: PathBuf,
}

impl PublishDirLayout {
    /// The layout rooted at `root`, `publish/<id>/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The new session's id: the directory name.
    pub fn id(&self) -> Option<&str> {
        self.root.file_name().and_then(|n| n.to_str())
    }

    pub fn attrs_file(&self) -> PathBuf {
        self.root.join(ATTRS_FILE)
    }

    /// The opaque content subtree, `files.d/`.
    pub fn files_dir(&self) -> PathBuf {
        self.root.join(FILES_TREE)
    }

    pub fn redacted_file(&self) -> PathBuf {
        self.root.join(REDACTED_FILE)
    }
}

/// The new session that `init` staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishInit {
    /// The new session's object id, the directory name and the handle
    /// for every later publish command.
    pub id: String,
    /// The `origin` the derived session records, `session:<id>@<commit>`.
    pub origin: String,
}

/// Publish operations over an opened store.
pub struct PublishStore<'a> {
    store: &'a Store,
    root: PathBuf,
}

impl<'a> From<&'a Store> for PublishStore<'a> {
    fn from(store: &'a Store) -> Self {
        PublishStore {
            store,
            root: publish_root(),
        }
    }
}

impl<'a> PublishStore<'a> {
    /// Stage the publish under `root` instead of Gage home. For tests.
    pub fn with_root(store: &'a Store, root: impl Into<PathBuf>) -> Self {
        PublishStore {
            store,
            root: root.into(),
        }
    }

    /// The definition file of the pending publish `id`,
    /// `publish/<id>.publish.json`: the instructions the staged session
    /// is derived from. Never committed.
    pub fn definition_file(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}{DEFINITION_SUFFIX}"))
    }

    /// The staging directory of the pending publish `id`.
    pub fn layout(&self, id: &str) -> PublishDirLayout {
        PublishDirLayout::new(self.root.join(id))
    }

    /// Start a publish of `original`, a stored session. Generates the
    /// new session id, creates the staging directory, copies
    /// `original`'s content into `files.d`, writes `attrs.json` with
    /// `origin` naming `original` and an empty `redacted.json`.
    /// Returns the new id. The definition beside the directory is the
    /// publish engine's to write. No store object is written.
    pub fn init(&self, original: &SessionRecord) -> Result<PublishInit, StoreError> {
        let id = new_uuid();
        let origin = format!("session:{}@{}", original.id, original.commit_sha);
        let layout = PublishDirLayout::new(self.root.join(&id));
        fs::create_dir_all(layout.files_dir()).map_err(StoreError::ReadContent)?;

        let content = SessionStore::from(self.store).content(&original.commit_sha);
        let mut size: u64 = 0;
        for path in content.paths().map_err(StoreError::ReadContent)? {
            let mut reader = content.open(&path).map_err(StoreError::ReadContent)?;
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(StoreError::ReadContent)?;
            size += bytes.len() as u64;
            let dest = layout.files_dir().join(&path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(StoreError::ReadContent)?;
            }
            fs::write(&dest, &bytes).map_err(StoreError::ReadContent)?;
        }

        // A derived session has no native artifact, so the fingerprint
        // fields are zero; nothing re-adds a derived session through
        // the path that reads them.
        let attrs = SessionAttrsRecord {
            driver: original.attrs.driver.clone(),
            origin: origin.clone(),
            native_mtime: 0,
            native_size: 0,
            session_type: original.attrs.session_type.clone(),
            content_format: original.attrs.content_format.clone(),
            size,
            project: original.attrs.project.clone(),
            summary: original.attrs.summary.clone(),
        };
        let value = serde_json::to_value(&attrs)
            .map_err(|e| StoreError::Parse(format!("publish attrs encode: {e}")))?;
        let mut attrs_bytes =
            serde_json::to_vec(&value).map_err(|e| StoreError::Parse(format!("{e}")))?;
        attrs_bytes.push(b'\n');
        fs::write(layout.attrs_file(), &attrs_bytes).map_err(StoreError::ReadContent)?;
        fs::write(layout.redacted_file(), b"[]\n").map_err(StoreError::ReadContent)?;

        Ok(PublishInit { id, origin })
    }

    /// The one pending publish whose id is `id_or_prefix` or starts
    /// with it. No match is `ObjectNotFound`; more than one is
    /// `AmbiguousId` naming the candidates.
    pub fn get(&self, id_or_prefix: &str) -> Result<PendingPublish, StoreError> {
        let mut matches: Vec<PendingPublish> = self
            .list()?
            .into_iter()
            .filter(|p| p.id.starts_with(id_or_prefix))
            .collect();
        match matches.len() {
            0 => Err(StoreError::ObjectNotFound(id_or_prefix.to_string())),
            1 => Ok(matches.remove(0)),
            _ => Err(StoreError::AmbiguousId(
                id_or_prefix.to_string(),
                matches
                    .into_iter()
                    .map(|p| IdMatch {
                        id: p.id,
                        tip_sha: String::new(),
                        object_type: OBJECT_TYPE.to_string(),
                        deleted: false,
                    })
                    .collect(),
            )),
        }
    }

    /// Delete the staging directory and the definition of the pending
    /// publish `id`. Nothing was written to the store, so nothing in
    /// the store changes. An unknown id is `ObjectNotFound`; a
    /// definition already deleted by hand is not an error.
    pub fn abort(&self, id: &str) -> Result<(), StoreError> {
        let root = self.root.join(id);
        match fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::ObjectNotFound(id.to_string()));
            }
            Err(e) => return Err(StoreError::ReadContent(e)),
        }
        match fs::remove_file(self.definition_file(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::ReadContent(e)),
        }
    }

    /// Every pending publish working tree under the publish root, in
    /// id order. A directory without a readable `attrs.json` is
    /// skipped, since it is mid-write or not a publish. Empty when the
    /// root does not exist.
    pub fn list(&self) -> Result<Vec<PendingPublish>, StoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::ReadContent(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let dir = entry.map_err(StoreError::ReadContent)?.path();
            if !dir.is_dir() {
                continue;
            }
            let layout = PublishDirLayout::new(&dir);
            let bytes = match fs::read(layout.attrs_file()) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StoreError::ReadContent(e)),
            };
            let attrs: SessionAttrsRecord = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Parse(format!("publish attrs {}: {e}", dir.display())))?;
            let Some(id) = layout.id().map(String::from) else {
                continue;
            };
            let modified_ms = fs::metadata(&dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push(PendingPublish {
                id,
                attrs,
                root: dir,
                modified_ms,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;

    #[test]
    fn init_stages_a_derived_session_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "hello\nworld\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();

        let publish_dir = tmp.path().join("publish");
        let init = PublishStore::with_root(&store, &publish_dir)
            .init(&original)
            .unwrap();

        let layout = PublishDirLayout::new(publish_dir.join(&init.id));
        assert_eq!(layout.id(), Some(init.id.as_str()));
        assert_eq!(
            init.origin,
            format!("session:{}@{}", original.id, original.commit_sha)
        );

        let attrs: SessionAttrsRecord =
            serde_json::from_slice(&fs::read(layout.attrs_file()).unwrap()).unwrap();
        assert_eq!(attrs.origin, init.origin);
        assert_eq!(attrs.driver, original.attrs.driver);
        assert_eq!(attrs.content_format, original.attrs.content_format);
        assert_eq!(attrs.size, 12);
        assert_eq!(attrs.native_mtime, 0);
        assert_eq!(attrs.native_size, 0);

        assert_eq!(fs::read_to_string(layout.redacted_file()).unwrap(), "[]\n");
        let staged = fs::read_to_string(layout.files_dir().join("session.jsonl")).unwrap();
        assert_eq!(staged, "hello\nworld\n");
    }

    #[test]
    fn list_returns_pending_working_trees_in_id_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();

        let publish_dir = tmp.path().join("publish");
        let publish = PublishStore::with_root(&store, &publish_dir);
        assert!(publish.list().unwrap().is_empty());

        let a = publish.init(&original).unwrap();
        let b = publish.init(&original).unwrap();

        let listed = publish.list().unwrap();
        assert_eq!(listed.len(), 2);
        let mut ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        let mut expected = vec![a.id.as_str(), b.id.as_str()];
        ids.sort_unstable();
        expected.sort_unstable();
        assert_eq!(ids, expected);
        assert!(listed.iter().all(|p| p.attrs.origin == a.origin));
    }

    /// `get` resolves a full id or a unique prefix, reports no match,
    /// and reports an ambiguous prefix with its candidates.
    #[test]
    fn get_resolves_a_pending_publish_by_id_or_unique_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish = PublishStore::with_root(&store, tmp.path().join("publish"));
        let a = publish.init(&original).unwrap();
        let b = publish.init(&original).unwrap();

        assert_eq!(publish.get(&a.id).unwrap().id, a.id);
        // Ids are random, so find a prefix of `a` that `b` does not share
        let unique =
            a.id.char_indices()
                .map(|(i, _)| &a.id[..=i])
                .find(|p| !b.id.starts_with(p))
                .unwrap();
        assert_eq!(publish.get(unique).unwrap().id, a.id);

        let missing = publish.get("zzzz").unwrap_err();
        assert!(matches!(missing, StoreError::ObjectNotFound(ref p) if p == "zzzz"));

        let ambiguous = publish.get("").unwrap_err();
        assert!(
            matches!(ambiguous, StoreError::AmbiguousId(_, ref c) if c.len() == 2),
            "{ambiguous}"
        );
    }

    /// `abort` deletes the staging directory and the definition and
    /// nothing else; a second abort of the same id is `ObjectNotFound`.
    #[test]
    fn abort_deletes_the_staging_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish_dir = tmp.path().join("publish");
        let publish = PublishStore::with_root(&store, &publish_dir);
        let a = publish.init(&original).unwrap();
        let b = publish.init(&original).unwrap();
        fs::write(publish.definition_file(&a.id), b"{}\n").unwrap();
        fs::write(publish.definition_file(&b.id), b"{}\n").unwrap();

        publish.abort(&a.id).unwrap();
        assert!(!publish_dir.join(&a.id).exists());
        assert!(!publish.definition_file(&a.id).exists());
        assert!(publish_dir.join(&b.id).exists());
        assert!(publish.definition_file(&b.id).exists());
        assert_eq!(publish.list().unwrap().len(), 1);
        assert_eq!(SessionStore::from(&store).iter().unwrap().count(), 1);

        let again = publish.abort(&a.id).unwrap_err();
        assert!(matches!(again, StoreError::ObjectNotFound(ref id) if *id == a.id));
    }
}
