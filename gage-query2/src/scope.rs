//! What a context is scoped to, and how session rows are reached.
//!
//! A context has store scope or scan scope. Store scope is every
//! live object in the store. Scan scope, a [`ScanScope`], is one
//! scan's objects: its dataset's sessions at the commits the scan
//! links, the notes it wrote or carried, the issues it wrote, and the
//! relations among them. The scan is an active one, named by its scan
//! directory, or a stored one, named by id; the tables read either
//! source on each query.
//!
//! The core knows the store's layout, so it resolves the session set
//! and plans every read. The driver that wrote a session is the only
//! party that can read its opaque bytes, so each session's rows come
//! from [`Driver::read_stored`] on that driver, normalized into
//! [`gage_session::Entry`] values the core turns into a batch.

use std::path::Path;
use std::sync::{Arc, Mutex};

use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;
use gage_session::Driver;
use gage_session::filter::IdFilter;
use gage_store::{
    Order, ScanDirLayout, ScanSource, SelectedTip, SessionRecord, SessionStore, Store, StoreError,
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

/// One stored session version selected for a scan
#[derive(Debug, Clone)]
pub struct StoredSessionRef {
    /// Gage object id; the value the rows carry as `session_id`
    pub id: String,
    /// Commit of the version read; the row cache key
    pub commit: String,
    /// The version's `created` and `modified` markers, UNIX millis
    pub created_ms: i64,
    pub modified_ms: i64,
    pub native_id: String,
    pub content_format: String,
    pub driver_name: String,
}

impl StoredSessionRef {
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
            commit: record.commit_sha,
            native_id: record.attrs.native_id,
            content_format: record.attrs.content_format,
            driver_name: record.driver_name,
        })
    }

    /// The version as the `session` table takes it.
    fn as_version(&self) -> SelectedTip {
        SelectedTip {
            id: self.id.clone(),
            sha: self.commit.clone(),
            created_ms: Some(self.created_ms),
            modified_ms: Some(self.modified_ms),
        }
    }
}

/// The sessions the row tables serve: every live session at its tip
/// in store scope, or the fixed versions of a scan scope's members.
pub struct SessionSet {
    store: Arc<Mutex<Store>>,
    drivers: DriverRegistry,
    fixed: Option<Vec<StoredSessionRef>>,
}

impl SessionSet {
    /// Every live session at its tip.
    pub(crate) fn all(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            fixed: None,
        }
    }

    /// Exactly `sessions`, each at the version it names.
    pub(crate) fn fixed(store: Arc<Mutex<Store>>, sessions: Vec<StoredSessionRef>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            fixed: Some(sessions),
        }
    }

    /// The versions of a fixed set, in order; `None` store-wide.
    pub(crate) fn fixed_versions(&self) -> Option<Vec<SelectedTip>> {
        self.fixed
            .as_ref()
            .map(|refs| refs.iter().map(StoredSessionRef::as_version).collect())
    }

    /// The set's sessions, narrowed by the `session_id` predicates in
    /// `filters`. A store-wide set lists every live session newest
    /// modified first; a fixed set keeps its own order.
    pub(crate) fn sessions(&self, filters: &[Expr]) -> Result<Vec<StoredSessionRef>> {
        if let Some(fixed) = &self.fixed {
            let mut sessions = fixed.clone();
            if let Some(id_filter) = IdFilter::new(filters, "session_id")? {
                sessions = id_filter.retain(sessions, |s| s.id.as_str())?;
            }
            return Ok(sessions);
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
        if let Some(batch) = cache.get(&session.commit) {
            return Ok(batch);
        }
        let driver = self.driver(&session.driver_name)?;
        let content = {
            let store = self.lock();
            SessionStore::from(&*store).content(&session.commit)
        };
        let mut stored = driver
            .read_stored(session.native_id.clone(), &session.content_format, content)
            .map_err(|e| DataFusionError::External(Box::new(e)))?;
        let batch = Arc::new(derive_batch(&session.id, stored.entries())?);
        cache.insert(&session.commit, Arc::clone(&batch));
        Ok(batch)
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
