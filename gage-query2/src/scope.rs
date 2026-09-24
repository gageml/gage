//! The stored sessions a query reads and how their rows are reached.
//!
//! The core knows the store's layout, so it resolves the session set
//! and plans every read. The driver that wrote a session is the only
//! party that can read its opaque bytes, so each session's rows come
//! from [`Driver::read_stored`] on that driver, normalized into
//! [`gage_session::Entry`] values the core turns into a batch.

use std::sync::{Arc, Mutex};

use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_registry::driver::DriverRegistry;
use gage_session::Driver;
use gage_session::filter::IdFilter;
use gage_store::{
    DatasetStore, Order, SelectedTip, SessionRecord, SessionStore, Store, StoreError,
};

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

/// The session set of a store-backed query context: every live
/// session at its tip, or a fixed set of versions such as a dataset's
/// members at the commit a scan links.
pub struct SessionScope {
    store: Arc<Mutex<Store>>,
    drivers: DriverRegistry,
    fixed: Option<Vec<StoredSessionRef>>,
}

impl SessionScope {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            fixed: None,
        }
    }

    /// A scope over exactly `sessions`, each at the version it names.
    pub fn with_sessions(store: Arc<Mutex<Store>>, sessions: Vec<StoredSessionRef>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
            fixed: Some(sessions),
        }
    }

    /// A scope over the members of the dataset at `dataset_commit`,
    /// each at the version the dataset links, in member order. This
    /// is the one read a scope makes outside the query interface: it
    /// pins the versions every table in the context then serves.
    pub fn for_dataset(store: Arc<Mutex<Store>>, dataset_commit: &str) -> Result<Self> {
        let members = {
            let guard = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            DatasetStore::from(&*guard)
                .sessions_at(dataset_commit)
                .map_err(external)?
        };
        Ok(Self::with_sessions(
            store,
            members
                .into_iter()
                .map(StoredSessionRef::from_record)
                .collect::<Result<_>>()?,
        ))
    }

    /// The versions of a fixed scope, in order; `None` store-wide.
    pub fn fixed_versions(&self) -> Option<Vec<SelectedTip>> {
        self.fixed
            .as_ref()
            .map(|refs| refs.iter().map(StoredSessionRef::as_version).collect())
    }

    /// The scope's sessions, narrowed by the `session_id` predicates
    /// in `filters`. A store-wide scope lists every live session
    /// newest modified first; a fixed scope keeps its own order.
    pub fn sessions(&self, filters: &[Expr]) -> Result<Vec<StoredSessionRef>> {
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
    pub fn rows(&self, session: &StoredSessionRef, cache: &RowCache) -> Result<Arc<RecordBatch>> {
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
