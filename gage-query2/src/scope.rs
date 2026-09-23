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
use gage_store::{Order, SessionStore, Store, StoreError};

use crate::rows::{RowCache, derive_batch};

/// One stored session version selected for a scan
#[derive(Debug, Clone)]
pub struct StoredSessionRef {
    /// Gage object id; the value the rows carry as `session_id`
    pub id: String,
    /// Commit of the version read; the row cache key
    pub commit: String,
    pub native_id: String,
    pub content_format: String,
    pub driver_name: String,
}

/// The session set of a store-backed query context
pub struct SessionScope {
    store: Arc<Mutex<Store>>,
    drivers: DriverRegistry,
}

impl SessionScope {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self {
            store,
            drivers: DriverRegistry::builtin(),
        }
    }

    /// Every live stored session, narrowed by the `session_id`
    /// predicates in `filters`, newest modified first.
    pub fn sessions(&self, filters: &[Expr]) -> Result<Vec<StoredSessionRef>> {
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
                Ok(StoredSessionRef {
                    id: record.id,
                    commit: record.commit_sha,
                    native_id: record.attrs.native_id,
                    content_format: record.attrs.content_format,
                    driver_name: record.driver_name,
                })
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
