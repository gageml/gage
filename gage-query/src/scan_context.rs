//! `ScanSessionContext` — DataFusion query surface bound to the
//! explicit set of sessions a scan run touches. Owns the
//! `SessionCache` extension so progress (`cached_session_count`)
//! can be read by the CLI while scanners issue SQL through the
//! wrapped `SessionContext`.

use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use datafusion::prelude::{SessionConfig, SessionContext};
use gage_claude::session::SessionInfo;

use crate::cache::SessionCache;
use crate::tables::{EntryTable, MessageTable};

pub struct ScanSessionContext {
    inner: SessionContext,
    cache: Arc<SessionCache>,
}

impl ScanSessionContext {
    pub fn new(selected: &Arc<[SessionInfo]>) -> Self {
        let cache = Arc::new(SessionCache::new());
        let config = SessionConfig::new()
            .with_extension(Arc::clone(&cache))
            .set_str("datafusion.sql_parser.dialect", "PostgreSQL");
        let inner = SessionContext::new_with_config(config);
        crate::install_udfs(&inner);

        let sessions: Arc<HashMap<String, PathBuf>> = Arc::new(
            selected
                .iter()
                .map(|s| (s.id.clone(), s.src.clone()))
                .collect(),
        );
        inner
            .register_table("entry", Arc::new(EntryTable::with_lookup(sessions.clone())))
            .unwrap();
        inner
            .register_table("message", Arc::new(MessageTable::with_lookup(sessions)))
            .unwrap();
        Self { inner, cache }
    }

    pub fn cached_session_count(&self) -> usize {
        self.cache.loaded()
    }
}

impl Deref for ScanSessionContext {
    type Target = SessionContext;
    fn deref(&self) -> &SessionContext {
        &self.inner
    }
}
