//! The staging of a pending publish.
//!
//! A publish is staged as a directory under Gage home,
//! `publish/<id>/`, named by the new session's object id. The
//! directory is a session directory: `session/` holds the derived
//! session's tree as the store will commit it, `attrs.json` with
//! `origin`, the `files.d` content, and `changes.json`. Beside it,
//! `publish.jsonc` is the publish definition: the instructions
//! the staged session is derived from, which may hold anything and is
//! never committed as written. The session enters the store only on
//! apply, which commits the session directory as it stands, keeps the
//! definition as an attachment, and then deletes the staging
//! directory; abort deletes it and writes nothing.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use gage_core::config::gage_home;
use gage_core::uuid::{is_id, new_uuid};
use gage_store::{
    AttachmentSpec, AttachmentStore, ExternalSession, ExternalSessionRecord, IdMatch, SESSION_TYPE,
    SessionAttrsRecord, SessionDirLayout, SessionRecord, SessionStore, Store, StoreError,
    format_changes,
};

use crate::PublishError;
use crate::definition::PublishDefinition;

const PUBLISH_DIR: &str = "publish";
const TMP_DIR: &str = "tmp";
const DEFINITION_SOURCE: &str = "publish.jsonc";
/// The definition as the attachment holds it: the definition's values
/// as JSON, without the comments that guide its editing
const DEFINITION_FILE: &str = "publish.json";
/// The name of the attachment that keeps a publish's definition once
/// the publish is applied
pub const DEFINITION_ATTACHMENT: &str = "publish-def";

/// The parent of publish staging directories, `publish/` under Gage
/// home.
pub fn publish_root() -> PathBuf {
    gage_home().join(PUBLISH_DIR)
}

/// The parent of the files an apply writes to attach, `tmp/` under
/// Gage home.
pub fn tmp_root() -> PathBuf {
    gage_home().join(TMP_DIR)
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

    /// The staged session's directory layout, whose tree is `session/`.
    pub fn session(&self) -> SessionDirLayout {
        SessionDirLayout::new(&self.root)
    }

    /// The definition file, `publish.jsonc`.
    pub fn definition_file(&self) -> PathBuf {
        self.root.join(DEFINITION_SOURCE)
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

/// The staging operations over an opened store.
pub struct Staging<'a> {
    store: &'a Store,
    root: PathBuf,
    tmp: PathBuf,
}

impl<'a> From<&'a Store> for Staging<'a> {
    fn from(store: &'a Store) -> Self {
        Staging {
            store,
            root: publish_root(),
            tmp: tmp_root(),
        }
    }
}

impl<'a> Staging<'a> {
    /// Stage under `root` instead of Gage home, with the apply's
    /// temporary files under `root/tmp`. For tests.
    pub fn with_root(store: &'a Store, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Staging {
            store,
            tmp: root.join(TMP_DIR),
            root,
        }
    }

    /// The staging directory of the pending publish `id`.
    pub fn layout(&self, id: &str) -> PublishDirLayout {
        PublishDirLayout::new(self.root.join(id))
    }

    /// The definition file of the pending publish `id`: the
    /// instructions the staged session is derived from.
    pub fn definition_file(&self, id: &str) -> PathBuf {
        self.layout(id).definition_file()
    }

    /// Start a publish of `original`, a stored session. Generates the
    /// new session id, creates the staging directory, copies
    /// `original`'s content into the session's `files.d`, writes
    /// `attrs.json` with `origin` naming `original` and an empty
    /// `changes.json`. Returns the new id. The definition beside the
    /// session is the publish engine's to write. No store object is
    /// written.
    pub fn init(&self, original: &SessionRecord) -> Result<PublishInit, PublishError> {
        let id = new_uuid();
        let origin = format!("session:{}@{}", original.id, original.commit_sha);
        let session = self.layout(&id).session();
        fs::create_dir_all(session.files_dir())?;

        let content = SessionStore::from(self.store).content(&original.commit_sha);
        let mut size: u64 = 0;
        for path in content.paths()? {
            let mut reader = content.open(&path)?;
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            size += bytes.len() as u64;
            let dest = session.files_dir().join(&path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, &bytes)?;
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
            project: None,
            summary: original.attrs.summary.clone(),
        };
        let value = serde_json::to_value(&attrs)?;
        let mut attrs_bytes = serde_json::to_vec(&value)?;
        attrs_bytes.push(b'\n');
        fs::write(session.attrs_file(), &attrs_bytes)?;
        fs::write(session.changes_file(), format_changes(&[]))?;

        Ok(PublishInit { id, origin })
    }

    /// The one pending publish whose id is `id_or_prefix` or starts
    /// with it. No match is `ObjectNotFound`; more than one is
    /// `AmbiguousId` naming the candidates.
    pub fn get(&self, id_or_prefix: &str) -> Result<ExternalSessionRecord, PublishError> {
        let mut matches: Vec<ExternalSessionRecord> = self
            .list()?
            .into_iter()
            .filter(|p| p.id.starts_with(id_or_prefix))
            .collect();
        match matches.len() {
            0 => Err(StoreError::ObjectNotFound(id_or_prefix.to_string()).into()),
            1 => Ok(matches.remove(0)),
            _ => Err(StoreError::AmbiguousId(
                id_or_prefix.to_string(),
                matches
                    .into_iter()
                    .map(|p| IdMatch {
                        id: p.id,
                        tip_sha: String::new(),
                        object_type: SESSION_TYPE.to_string(),
                        deleted: false,
                    })
                    .collect(),
            )
            .into()),
        }
    }

    /// Every pending publish under the publish root, read, in id
    /// order.
    pub fn list(&self) -> Result<Vec<ExternalSessionRecord>, PublishError> {
        self.external()?
            .iter()
            .map(|session| Ok(session.read()?))
            .collect()
    }

    /// Every pending publish under the publish root as the external
    /// session it is, in id order: each id-named directory whose
    /// staged `attrs.json` is present. A directory without one is
    /// mid-write or not a publish. Empty when the root does not exist.
    pub fn external(&self) -> Result<Vec<ExternalSession>, PublishError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let dir = entry?.path();
            let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_id(id) || !dir.is_dir() {
                continue;
            }
            if !SessionDirLayout::new(&dir).attrs_file().is_file() {
                continue;
            }
            out.push(ExternalSession {
                id: id.to_string(),
                dir,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Commit the staged session of the pending publish `id` as a new
    /// session object, as it stands. With `definition`, its values
    /// are kept as an unkeyed attachment named `publish-def` whose
    /// one target is the new session, written as JSON without the
    /// editing comments; the attachment's commit names the session's
    /// as a parent and not the reverse, so a push or fetch of the
    /// session does not carry the definition. The staging directory
    /// is deleted afterward. Returns the commit SHA. Nothing in the
    /// staged content is checked here; the publish engine validates
    /// before it calls.
    pub fn apply(
        &self,
        id: &str,
        definition: Option<&PublishDefinition>,
    ) -> Result<String, PublishError> {
        let layout = self.layout(id);
        if !layout.root().is_dir() {
            return Err(StoreError::ObjectNotFound(id.to_string()).into());
        }
        let attachment = match definition {
            Some(definition) => {
                let dir = self.tmp.join(PUBLISH_DIR).join(id);
                fs::create_dir_all(&dir)?;
                let mut text = serde_json::to_string_pretty(definition)?;
                text.push('\n');
                fs::write(dir.join(DEFINITION_FILE), text)?;
                Some(dir)
            }
            None => None,
        };
        let outcome = self.commit(id, &layout, attachment.as_deref());
        if let Some(dir) = &attachment {
            fs::remove_dir_all(dir)?;
        }
        let commit_sha = outcome?;
        self.remove_staging(id)?;
        Ok(commit_sha)
    }

    /// The commit of an apply: the session, then the attachment of
    /// the definition written under `attachment`.
    fn commit(
        &self,
        id: &str,
        layout: &PublishDirLayout,
        attachment: Option<&Path>,
    ) -> Result<String, PublishError> {
        let commit_sha = SessionStore::from(self.store).create_from_dir(layout.root())?;
        if let Some(dir) = attachment {
            AttachmentStore::from(self.store).add(&AttachmentSpec {
                name: Some(DEFINITION_ATTACHMENT),
                key: None,
                targets: &[format!("session:{id}")],
                root: dir,
                includes: &[DEFINITION_FILE.to_string()],
                excludes: &[],
            })?;
        }
        Ok(commit_sha)
    }

    /// Delete the staging directory of the pending publish `id`, the
    /// definition with it. Nothing was written to the store, so
    /// nothing in the store changes. An unknown id is `ObjectNotFound`.
    pub fn abort(&self, id: &str) -> Result<(), PublishError> {
        self.remove_staging(id)
    }

    fn remove_staging(&self, id: &str) -> Result<(), PublishError> {
        match fs::remove_dir_all(self.root.join(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(StoreError::ObjectNotFound(id.to_string()).into())
            }
            Err(e) => Err(e.into()),
        }
    }
}
