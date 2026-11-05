//! What a context is scoped to, and how session rows are reached.
//!
//! A context has store scope, scan scope, or external scope. Store
//! scope is every live object in the store. Scan scope, a
//! [`ScanScope`], is one scan's objects: its dataset's sessions at the
//! commits the scan links, the notes it wrote or carried, the issues
//! it wrote, and the relations among them. The scan is an active one,
//! named by its scan directory, or a stored one, named by id; the
//! tables read either source on each query. External scope is a
//! given set of session directories, read from the file system on
//! each query, so a rewritten directory is served on the next read.
//!
//! The core knows the store's layout, so it resolves the session set
//! and plans every read. The driver that wrote a session is the only
//! party that can read its opaque bytes, so each session's rows come
//! from [`Driver::read_stored`] on that driver, normalized into
//! [`gage_session::Entry`] values the core turns into a batch.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;
use gage_session::filter::IdFilter;
use gage_session::{ContentSource, Driver, StoredSession, tool_use_batch, tool_use_file_batch};
use gage_store::{
    DirContentSource, ExternalSession, ExternalSessionRecord, Order, ScanDirLayout, ScanSource,
    SelectedTip, SessionDirLayout, SessionRecord, SessionStore, Store, StoreError,
};

/// One scan's objects, as a context is scoped to them.
#[derive(Debug, Clone)]
pub struct ScanScope {
    source: ScanSource,
    /// Narrow the sessions to this one member
    session: Option<String>,
}

impl ScanScope {
    /// The scope of an active scan, from its scan directory.
    pub fn scan_dir(root: impl AsRef<Path>) -> Self {
        Self {
            source: ScanSource::ScanDir(ScanDirLayout::new(root.as_ref())),
            session: None,
        }
    }

    /// The scope of a stored scan.
    pub fn stored(scan_id: impl Into<String>) -> Self {
        Self {
            source: ScanSource::Stored(scan_id.into()),
            session: None,
        }
    }

    /// A dataset alone, at `commit_sha`: the session tables serve its
    /// members and attachments and the scan tables are empty.
    pub fn dataset(commit_sha: impl Into<String>) -> Self {
        Self {
            source: ScanSource::Dataset(commit_sha.into()),
            session: None,
        }
    }

    /// Narrow `session`, `entry`, and `message` to the one member
    /// `session_id`. Every other table stays the whole scan's.
    pub fn session(mut self, session_id: impl Into<String>) -> Self {
        self.session = Some(session_id.into());
        self
    }

    pub(crate) fn source(&self) -> &ScanSource {
        &self.source
    }

    /// The scope's sessions at the commits the scan links, in member
    /// order, narrowed when a session was named. A named session
    /// that is not a member is an error.
    pub(crate) fn members(&self, store: &Store) -> Result<Vec<StoredSessionRef>> {
        let mut members = self
            .source
            .members(store)?
            .into_iter()
            .map(StoredSessionRef::from_record)
            .collect::<Result<Vec<_>>>()?;
        if let Some(id) = &self.session {
            members.retain(|m| m.id == *id);
            if members.is_empty() {
                return Err(DataFusionError::Plan(format!(
                    "session {id} is not a member of the scan"
                )));
            }
        }
        Ok(members)
    }
}

use crate::rows::{RowCache, derive_batch};
use crate::session_change::session_change_batch;

/// One session selected for a scan
#[derive(Debug, Clone)]
pub struct StoredSessionRef {
    /// Gage object id; the value the rows carry as `session_id`
    pub id: String,
    pub source: SessionSource,
    /// The `type` blob version at the commit; empty for an external
    /// session
    pub type_version: String,
    /// The version's `created` and `modified` markers, UNIX millis;
    /// the `attrs.json` mtime for an external session
    pub created_ms: i64,
    pub modified_ms: i64,
    pub native_id: String,
    pub content_format: String,
    pub driver_name: String,
}

/// Where a session's bytes are read from.
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// A committed object. The commit is the session's version, and
    /// a version never changes, so it is also the cache key of the
    /// rows derived from it.
    Stored { commit: String },
    /// A session directory. It has no version: the directory is
    /// mutable and nothing names a content state. The `attrs.json`
    /// mtime read with the listing invalidates the rows derived from
    /// it, since a writer that changes the session rewrites the file.
    External {
        dir: PathBuf,
        attrs_modified_ms: i64,
    },
}

impl StoredSessionRef {
    /// The key the row caches hold this session's derived batches
    /// under: the commit of a stored session, or the id and
    /// `attrs.json` mtime of an external one.
    pub(crate) fn cache_key(&self) -> String {
        match &self.source {
            SessionSource::Stored { commit } => commit.clone(),
            SessionSource::External {
                attrs_modified_ms, ..
            } => format!("external:{}@{attrs_modified_ms}", self.id),
        }
    }

    /// The commit of a stored session; `None` for an external one,
    /// which has no version.
    pub(crate) fn commit(&self) -> Option<&str> {
        match &self.source {
            SessionSource::Stored { commit } => Some(commit),
            SessionSource::External { .. } => None,
        }
    }

    /// The reference to a session record at the commit it was read.
    /// A record without its markers is a malformed object.
    pub fn from_record(record: SessionRecord) -> Result<Self> {
        let marker = |name: &str, value: Option<i64>| {
            value.ok_or_else(|| {
                external(StoreError::Parse(format!(
                    "session {}: missing {name} marker",
                    record.id
                )))
            })
        };
        Ok(StoredSessionRef {
            created_ms: marker("created", record.created_ms)?,
            modified_ms: marker("modified", record.modified_ms)?,
            id: record.id,
            source: SessionSource::Stored {
                commit: record.commit_sha,
            },
            type_version: record.type_version,
            native_id: record.attrs.native_id().to_string(),
            content_format: record.attrs.content_format,
            driver_name: record.driver_name,
        })
    }

    /// The reference to an external session as one listing read it.
    /// Its content comes from the directory's `files.d`, and its
    /// created and modified are the `attrs.json` mtime of the read.
    fn from_external(record: ExternalSessionRecord) -> Self {
        let driver_name = record
            .attrs
            .driver
            .split_once(' ')
            .map(|(name, _)| name)
            .unwrap_or(&record.attrs.driver)
            .to_string();
        StoredSessionRef {
            source: SessionSource::External {
                dir: record.dir,
                attrs_modified_ms: record.attrs_modified_ms,
            },
            type_version: String::new(),
            created_ms: record.attrs_modified_ms,
            modified_ms: record.attrs_modified_ms,
            native_id: record.attrs.native_id().to_string(),
            content_format: record.attrs.content_format,
            driver_name,
            id: record.id,
        }
    }

    /// The version as the `session` table takes it. A fixed set holds
    /// stored sessions only.
    fn as_version(&self) -> SelectedTip {
        SelectedTip {
            id: self.id.clone(),
            sha: self
                .commit()
                .expect("a fixed set is built from stored sessions")
                .to_string(),
            version: self.type_version.clone(),
            created_ms: Some(self.created_ms),
            modified_ms: Some(self.modified_ms),
        }
    }
}

/// The sessions the row tables serve: every live session at its tip
/// in store scope, the fixed versions of a scan scope's members, or
/// the session directories of an external scope.
pub struct SessionSet {
    store: Arc<Mutex<Store>>,
    drivers: DriverRegistry,
    members: Members,
}

enum Members {
    /// Every live session at its tip
    All,
    /// Exactly these sessions, each at the version it names
    Fixed(Vec<StoredSessionRef>),
    /// Exactly these session directories, each read on each listing
    External(Vec<ExternalSession>),
}

impl SessionSet {
    /// Every live session at its tip.
    pub fn all(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            members: Members::All,
        }
    }

    /// Exactly `sessions`, each at the version it names.
    pub(crate) fn fixed(store: Arc<Mutex<Store>>, sessions: Vec<StoredSessionRef>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            members: Members::Fixed(sessions),
        }
    }

    /// Exactly `sessions`, session directories read on each listing.
    pub(crate) fn external(store: Arc<Mutex<Store>>, sessions: Vec<ExternalSession>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            members: Members::External(sessions),
        }
    }

    /// The versions of a fixed set, in order; `None` for any other
    /// set.
    pub(crate) fn fixed_versions(&self) -> Option<Vec<SelectedTip>> {
        match &self.members {
            Members::Fixed(refs) => Some(refs.iter().map(StoredSessionRef::as_version).collect()),
            Members::All | Members::External(_) => None,
        }
    }

    /// The set's sessions, narrowed by the `session_id` predicates in
    /// `filters`. A store-wide set lists every live session newest
    /// modified first; a fixed set keeps its own order; an external
    /// set lists its directories newest modified first, each read
    /// again so a rewrite changes its cache key.
    pub(crate) fn sessions(&self, filters: &[Expr]) -> Result<Vec<StoredSessionRef>> {
        match &self.members {
            Members::Fixed(fixed) => {
                let mut sessions = fixed.clone();
                if let Some(id_filter) = IdFilter::new(filters, "session_id")? {
                    sessions = id_filter.retain(sessions, |s| s.id.as_str())?;
                }
                return Ok(sessions);
            }
            Members::External(externals) => {
                let mut records = externals
                    .iter()
                    .map(ExternalSession::read)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(external)?;
                records.sort_by_key(|r| Reverse(r.attrs_modified_ms));
                let mut sessions: Vec<StoredSessionRef> = records
                    .into_iter()
                    .map(StoredSessionRef::from_external)
                    .collect();
                if let Some(id_filter) = IdFilter::new(filters, "session_id")? {
                    sessions = id_filter.retain(sessions, |s| s.id.as_str())?;
                }
                return Ok(sessions);
            }
            Members::All => {}
        }
        let store = self.lock();
        let sessions = SessionStore::from(&*store);
        let mut tips = sessions
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        if let Some(id_filter) = IdFilter::new(filters, "session_id")? {
            tips = id_filter.retain(tips, |t| t.id.as_str())?;
        }
        tips.into_iter()
            .map(|tip| {
                let record = sessions.at_commit(&tip.sha).map_err(external)?;
                StoredSessionRef::from_record(record)
            })
            .collect()
    }

    /// The derived batch of `session`, read through its driver on
    /// first use and served from `cache` after.
    pub(crate) fn rows(
        &self,
        session: &StoredSessionRef,
        cache: &RowCache,
    ) -> Result<Arc<RecordBatch>> {
        let key = session.cache_key();
        if let Some(batch) = cache.get(&key) {
            return Ok(batch);
        }
        let mut stored = self.read_stored(session)?;
        let batch = Arc::new(derive_batch(&session.id, stored.entries())?);
        cache.insert(&key, Arc::clone(&batch));
        Ok(batch)
    }

    /// The `tool_use` batch of `session`, read and cached as [`rows`]
    /// reads the derived batch.
    ///
    /// [`rows`]: SessionSet::rows
    pub(crate) fn tool_uses(
        &self,
        session: &StoredSessionRef,
        cache: &RowCache,
    ) -> Result<Arc<RecordBatch>> {
        let key = session.cache_key();
        if let Some(batch) = cache.get_tool_uses(&key) {
            return Ok(batch);
        }
        let mut stored = self.read_stored(session)?;
        let batch = Arc::new(tool_use_batch(&session.id, stored.tool_uses())?);
        cache.insert_tool_uses(&key, Arc::clone(&batch));
        Ok(batch)
    }

    /// The `tool_use_file` batch of `session`, read and cached as
    /// [`rows`] reads the derived batch.
    ///
    /// [`rows`]: SessionSet::rows
    pub(crate) fn tool_use_files(
        &self,
        session: &StoredSessionRef,
        cache: &RowCache,
    ) -> Result<Arc<RecordBatch>> {
        let key = session.cache_key();
        if let Some(batch) = cache.get_tool_use_files(&key) {
            return Ok(batch);
        }
        let mut stored = self.read_stored(session)?;
        let batch = Arc::new(tool_use_file_batch(&session.id, stored.tool_uses())?);
        cache.insert_tool_use_files(&key, Arc::clone(&batch));
        Ok(batch)
    }

    /// The `session_change` batch of `session`: the records of its
    /// `changes.json`, from the session directory of an external
    /// session or the blob of a commit, cached as [`rows`] caches the
    /// derived batch.
    ///
    /// [`rows`]: SessionSet::rows
    pub(crate) fn session_changes(
        &self,
        session: &StoredSessionRef,
        cache: &RowCache,
    ) -> Result<Arc<RecordBatch>> {
        let key = session.cache_key();
        if let Some(batch) = cache.get_session_changes(&key) {
            return Ok(batch);
        }
        let records = match &session.source {
            SessionSource::External { dir, .. } => SessionDirLayout::new(dir).read_changes(),
            SessionSource::Stored { commit } => {
                let store = self.lock();
                SessionStore::from(&*store).changes(commit)
            }
        }
        .map_err(external)?;
        let batch = Arc::new(session_change_batch(&session.id, &records)?);
        cache.insert_session_changes(&key, Arc::clone(&batch));
        Ok(batch)
    }

    fn read_stored(&self, session: &StoredSessionRef) -> Result<Box<dyn StoredSession>> {
        let driver = self.driver(&session.driver_name)?;
        let content: Box<dyn ContentSource> = match &session.source {
            SessionSource::External { dir, .. } => Box::new(DirContentSource::new(
                SessionDirLayout::new(dir).files_dir(),
            )),
            SessionSource::Stored { commit } => {
                let store = self.lock();
                SessionStore::from(&*store).content(commit)
            }
        };
        driver
            .read_stored(session.native_id.clone(), &session.content_format, content)
            .map_err(|e| DataFusionError::External(Box::new(e)))
    }

    fn driver(&self, name: &str) -> Result<Arc<dyn Driver>> {
        self.drivers.for_name(name).ok_or_else(|| {
            DataFusionError::Execution(format!("no driver named {name:?} is registered"))
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn external(e: StoreError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}
