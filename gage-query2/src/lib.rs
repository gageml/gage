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
//!
//! Session tables carry [`system_cols`], the columns a program needs
//! to render or address a row. A context includes them by default;
//! [`ContextBuilder::skip_system_cols`] hides them for a context that
//! serves people and models writing queries.

mod native_session;
mod project;
pub mod system_cols;

use std::sync::{Arc, Mutex};

use datafusion::datasource::TableProvider;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_query::SessionCache;
use gage_store::{Store, StoredSessionTable};

use crate::native_session::NativeSessionFn;
use crate::project::project_for_path_udf;
use crate::system_cols::SkipSystemCols;

/// Composes a query context. `store` backs the `session` table when
/// present; without it, `session` is absent and only the native
/// discovery functions are available.
pub struct ContextBuilder {
    store: Option<Arc<Mutex<Store>>>,
    skip_system_cols: bool,
}

impl ContextBuilder {
    pub fn new(store: Option<Arc<Mutex<Store>>>) -> Self {
        Self {
            store,
            skip_system_cols: false,
        }
    }

    /// Hide the system columns of every session table. The columns
    /// remain in the data; they leave the schema that `SELECT *`,
    /// `DESCRIBE`, and `information_schema` report.
    pub fn skip_system_cols(mut self) -> Self {
        self.skip_system_cols = true;
        self
    }

    pub fn build(self) -> SessionContext {
        let ctx = new_context(self.skip_system_cols);
        if let Some(store) = self.store {
            let table: Arc<dyn TableProvider> = Arc::new(StoredSessionTable::new(store));
            let table = if self.skip_system_cols {
                Arc::new(SkipSystemCols::new(table))
            } else {
                table
            };
            ctx.register_table("session", table)
                .expect("register session table on a fresh context");
        }
        ctx
    }
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
fn new_context(skip_system_cols: bool) -> SessionContext {
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
    ctx.register_udtf(
        "native_session",
        Arc::new(NativeSessionFn { skip_system_cols }),
    );
    ctx.register_udf(project_for_path_udf());
    ctx
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::array::StringArray;
    use datafusion::prelude::SessionContext;
    use gage_store::Store;
    use tempfile::TempDir;

    use super::ContextBuilder;

    /// A fresh store and the guard keeping its directory alive
    fn open_store() -> (TempDir, Arc<Mutex<Store>>) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open(&path).unwrap();
        (tmp, Arc::new(Mutex::new(store)))
    }

    async fn strings(ctx: &SessionContext, sql: &str) -> Vec<String> {
        let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
        batches
            .iter()
            .flat_map(|b| {
                let col = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                (0..b.num_rows())
                    .map(|i| col.value(i).to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// A store-backed context registers `session` and exposes it
    /// through `information_schema`, so the REPL's `\d` finds it.
    #[tokio::test]
    async fn store_backed_context_registers_the_session_table() {
        let (_tmp, store) = open_store();
        let ctx = ContextBuilder::new(Some(store)).build();
        let names = strings(
            &ctx,
            "SELECT table_name FROM information_schema.tables WHERE table_name = 'session'",
        )
        .await;
        assert_eq!(names, ["session"]);
    }

    /// The default context lists the system columns after the
    /// user-facing columns; `skip_system_cols` removes them from the
    /// reported schema.
    #[tokio::test]
    async fn skip_system_cols_hides_them_from_the_schema() {
        let sql = "SELECT column_name FROM information_schema.columns \
                   WHERE table_name = 'session' ORDER BY ordinal_position";

        let (_tmp, store) = open_store();
        let ctx = ContextBuilder::new(Some(store)).build();
        let cols = strings(&ctx, sql).await;
        assert_eq!(
            &cols[cols.len() - 3..],
            ["id_display", "id_prefix", "locator"]
        );

        let (_tmp, store) = open_store();
        let ctx = ContextBuilder::new(Some(store)).skip_system_cols().build();
        let cols = strings(&ctx, sql).await;
        assert_eq!(cols.first().map(String::as_str), Some("id"));
        assert!(
            !cols
                .iter()
                .any(|c| c == "id_display" || c == "id_prefix" || c == "locator")
        );
    }
}
