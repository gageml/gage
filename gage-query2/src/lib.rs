//! Composed DataFusion query surface over Gage data.
//!
//! gage-query2 is the query orchestrator. It owns `SessionContext`
//! creation and the context-level configuration --- the `SessionCache`
//! extension, the SQL dialect, `information_schema`, and the UDF suite
//! --- and composes what the data crates and drivers contribute. A
//! data crate owns its providers; this crate names them and registers
//! them onto the one context a query runs against.
//!
//! Named tables are store data. `session` is one row per live stored
//! session object, from [`gage_store::StoredSessionTable`]; `note`,
//! `dataset`, `scan`, and `issue` join it as the crate grows.
//!
//! Native session data is reached through driver-provided functions,
//! present on every context: the [`native_session`] table function
//! lists a source's native sessions, and the [`project`]
//! `project_for_path` UDF maps a directory to its project name for
//! filtering. Both are how a user discovers sessions to add to the
//! store; neither is a named table.

mod native_session;
mod project;

use std::sync::{Arc, Mutex};

use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_query::SessionCache;
use gage_store::{Store, StoredSessionTable};

use crate::native_session::NativeSessionFn;
use crate::project::project_for_path_udf;

/// Compose a query context. `store` backs the `session` table when
/// present; without it, `session` is absent and only the native
/// discovery functions are available.
pub fn context(store: Option<Arc<Mutex<Store>>>) -> SessionContext {
    let ctx = new_context();
    if let Some(store) = store {
        ctx.register_table("session", Arc::new(StoredSessionTable::new(store)))
            .expect("register session table on a fresh context");
    }
    ctx
}

/// The table functions this crate's contexts expose, for the repl's
/// `\df`. `native_session`'s result columns depend on the source's
/// driver, so it advertises no fixed schema.
pub fn repl_functions() -> Vec<gage_query::tables::TvfInfo> {
    vec![gage_query::tables::TvfInfo {
        name: "native_session",
        args: "[source text]",
        schema: None,
    }]
}

/// A context carrying the shared configuration --- the `SessionCache`
/// extension, the PostgreSQL SQL dialect, `information_schema`, the
/// `native_session` table function, and the `project_for_path` UDF ---
/// with no named tables registered. It carries only what this crate
/// installs; gage-query's function suite is not pulled in.
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
    ctx.register_udtf("native_session", Arc::new(NativeSessionFn));
    ctx.register_udf(project_for_path_udf());
    ctx
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::array::StringArray;
    use gage_store::Store;

    use super::context;

    /// A store-backed context registers `session` and exposes it
    /// through `information_schema`, so the REPL's `\d` finds it.
    #[tokio::test]
    async fn store_backed_context_registers_the_session_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();

        let ctx = context(Some(Arc::new(Mutex::new(store))));
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
