//! Composed DataFusion query surface over Gage data.
//!
//! gage-query2 is the query orchestrator. It owns `SessionContext`
//! creation and the context-level configuration --- the `SessionCache`
//! extension, the SQL dialect, `information_schema`, and the UDF suite
//! --- and composes the `TableProvider`s that the data crates
//! contribute. A data crate owns its providers; this crate names them
//! and registers them onto the one context a query runs against.
//!
//! The surface has a single `session` table. Its backing is a choice,
//! expressed by [`SessionBacking`], because one `session` table cannot
//! be the store and a driver source at once. Two backings exist: the
//! Gage store, and a driver source. The store backing registers only
//! `session`; the source backing registers the driver's `session`,
//! `message`, and `entry` tables. The gage-db state tables and agent
//! scope join [`ContextBuilder`] as the crate absorbs the query paths
//! gage-query serves now.

use std::sync::{Arc, Mutex};

use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_query::{SessionCache, install_udfs};
use gage_session::{DriverError, Source};
use gage_store::{Store, StoredSessionTable};

/// What backs the single `session` table in the composed surface.
pub enum SessionBacking<'a> {
    /// The Gage store: one row per live session object, served by
    /// [`gage_store::StoredSessionTable`]. Registers `session` only.
    Store(Arc<Mutex<Store>>),
    /// A driver source: the native sessions a [`Source`] exposes.
    /// Registers the driver's `session`, `message`, and `entry`
    /// tables.
    Source(&'a dyn Source),
}

/// Compose a query context whose `session` table is backed as
/// `sessions` specifies, with the shared context configuration
/// applied. The entry point for a client that wants a query surface
/// without wiring providers itself.
pub fn context(sessions: SessionBacking<'_>) -> Result<SessionContext, DriverError> {
    ContextBuilder::new(sessions).build()
}

/// Builds a composed [`SessionContext`]. Constructed with the
/// `session` backing; further sources join through builder methods as
/// they are implemented.
pub struct ContextBuilder<'a> {
    sessions: SessionBacking<'a>,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(sessions: SessionBacking<'a>) -> Self {
        Self { sessions }
    }

    /// Create the context, apply the shared configuration, and
    /// register the composed providers.
    pub fn build(self) -> Result<SessionContext, DriverError> {
        let ctx = new_context();
        match self.sessions {
            SessionBacking::Store(store) => {
                ctx.register_table("session", Arc::new(StoredSessionTable::new(store)))
                    .expect("register session table on a fresh context");
            }
            SessionBacking::Source(source) => {
                let tables = source.tables()?;
                ctx.register_table("session", tables.session)
                    .expect("register session table on a fresh context");
                ctx.register_table("message", tables.message)
                    .expect("register message table on a fresh context");
                ctx.register_table("entry", tables.entry)
                    .expect("register entry table on a fresh context");
            }
        }
        Ok(ctx)
    }
}

/// A context carrying the shared configuration --- the `SessionCache`
/// extension, the PostgreSQL SQL dialect, `information_schema`, and the
/// UDF suite --- with no tables registered.
fn new_context() -> SessionContext {
    let cache = Arc::new(SessionCache::new());
    let config = SessionConfig::new()
        .with_information_schema(true)
        .with_extension(cache)
        .set_str("datafusion.sql_parser.dialect", "PostgreSQL");
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_default_features()
        .build();
    let ctx = SessionContext::new_with_state(state);
    install_udfs(&ctx);
    ctx
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::array::StringArray;
    use gage_store::Store;

    use super::{SessionBacking, context};

    /// The composed context registers `session` and exposes it through
    /// `information_schema`, so the REPL's `\d` finds it.
    #[tokio::test]
    async fn store_backed_context_registers_the_session_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();

        let ctx = context(SessionBacking::Store(Arc::new(Mutex::new(store)))).unwrap();
        let batches = ctx
            .sql("SELECT table_name FROM information_schema.tables WHERE table_name = 'session'")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        let batch = batches.first().unwrap();
        let names = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(names.value(0), "session");
    }
}
