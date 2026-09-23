//! Composed DataFusion query surface over Gage data.
//!
//! gage-query2 is the query orchestrator. It owns `SessionContext`
//! creation and the context-level configuration --- the `SessionCache`
//! and [`rows::RowCache`] extensions, the SQL dialect,
//! `information_schema`, and the UDF suite --- and composes what the
//! data crates and drivers contribute.
//!
//! Named tables are store data. `session` is one row per live stored
//! session object, from [`gage_store::StoredSessionTable`]. `entry`
//! and `message` are the rows of those sessions: the core resolves
//! the session set and plans every read ([`scope`]), and the driver
//! that wrote each session deserializes its bytes into normalized
//! entries the core turns into batches ([`rows`], [`stored_rows`]).
//! `note` is one row per live note object, from
//! [`gage_store::StoredNoteTable`]. `dataset`, `scan`, and `issue`
//! join as the crate grows.
//!
//! Native session data is reached through driver-provided functions,
//! present on every context: `native_session`, `native_message`, and
//! `native_entry` ([`native`]) return a source's native tables, and
//! the [`project`] `project_for_path` UDF maps a directory to its
//! project name for filtering. These are how a user discovers
//! sessions to add to the store; none is a named table.
//!
//! Session tables carry [`system_cols`], the columns a program needs
//! to render or address a row. A context includes them by default;
//! [`ContextBuilder::skip_system_cols`] hides them for a context that
//! serves people and models writing queries.

mod native;
mod project;
pub mod rows;
pub mod scope;
pub mod stored_rows;
pub mod system_cols;

use std::sync::{Arc, Mutex};

use datafusion::datasource::TableProvider;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_query::SessionCache;
use gage_store::{Store, StoredNoteTable, StoredSessionTable};

use crate::native::{NativeTable, NativeTableFn};
use crate::project::project_for_path_udf;
use crate::rows::RowCache;
use crate::scope::SessionScope;
use crate::stored_rows::{RowKind, StoredRowsTable};
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
            let session: Arc<dyn TableProvider> =
                Arc::new(StoredSessionTable::new(Arc::clone(&store)));
            let note: Arc<dyn TableProvider> = Arc::new(StoredNoteTable::new(Arc::clone(&store)));
            let scope = Arc::new(SessionScope::new(store));
            let entry: Arc<dyn TableProvider> =
                Arc::new(StoredRowsTable::new(RowKind::Entry, Arc::clone(&scope)));
            let message: Arc<dyn TableProvider> =
                Arc::new(StoredRowsTable::new(RowKind::Message, scope));
            for (name, table) in [
                ("session", session),
                ("entry", entry),
                ("message", message),
                ("note", note),
            ] {
                let table = if self.skip_system_cols {
                    Arc::new(SkipSystemCols::new(table))
                } else {
                    table
                };
                ctx.register_table(name, table)
                    .expect("register store tables on a fresh context");
            }
        }
        ctx
    }
}

/// The table functions this crate's contexts expose, for the repl's
/// `\df`. A native table's columns depend on the source's driver, so
/// none advertises a fixed schema.
pub fn repl_functions() -> Vec<gage_query::tables::TvfInfo> {
    NATIVE_TABLES
        .iter()
        .map(|t| gage_query::tables::TvfInfo {
            name: t.function_name(),
            args: "[source text]",
            schema: None,
        })
        .collect()
}

const NATIVE_TABLES: [NativeTable; 3] = [
    NativeTable::Session,
    NativeTable::Message,
    NativeTable::Entry,
];

/// A context carrying the shared configuration --- the `SessionCache`
/// and `RowCache` extensions, the PostgreSQL SQL dialect,
/// `information_schema`, the native table functions, and the
/// `project_for_path` UDF --- with no named tables registered. It
/// carries only what this crate installs; gage-query's function suite
/// is not pulled in.
fn new_context(skip_system_cols: bool) -> SessionContext {
    let config = SessionConfig::new()
        .with_information_schema(true)
        .with_extension(Arc::new(SessionCache::new()))
        .with_extension(Arc::new(RowCache::new()))
        .set_str("datafusion.sql_parser.dialect", "PostgreSQL");
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_default_features()
        .build();
    let ctx = SessionContext::new_with_state(state);
    for table in NATIVE_TABLES {
        ctx.register_udtf(
            table.function_name(),
            Arc::new(NativeTableFn {
                table,
                skip_system_cols,
            }),
        );
    }
    ctx.register_udf(project_for_path_udf());
    ctx
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::array::StringArray;
    use datafusion::prelude::SessionContext;
    use gage_registry::driver::DriverRegistry;
    use gage_store::{SessionStore, Store};
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

    /// `entry` and `message` serve a stored session's rows through
    /// the driver that wrote it, keyed by the Gage session id so they
    /// join to `session`.
    #[tokio::test]
    async fn entry_and_message_read_stored_sessions_through_the_driver() {
        let (_tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-555555555555";
        let project_dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"hello there"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"type":"summary","summary":"s","leafUuid":"a1"}"#,
                "\n",
            ),
        )
        .unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver.open_source(&spec).unwrap();
        let mut native = source.open_native(native_id).unwrap();
        {
            let store = store.lock().unwrap();
            SessionStore::from(&*store)
                .add(driver.as_ref(), native.as_mut())
                .unwrap();
        }

        let ctx = ContextBuilder::new(Some(store)).build();
        let types = strings(&ctx, "SELECT type FROM entry ORDER BY line").await;
        assert_eq!(types, ["user", "assistant", "summary"]);

        let texts = strings(&ctx, "SELECT text FROM message ORDER BY line").await;
        assert_eq!(texts, ["hello there", "hi"]);

        let joined = strings(
            &ctx,
            "SELECT m.text FROM message m JOIN session s ON m.session_id = s.id \
             WHERE m.line = 2",
        )
        .await;
        assert_eq!(joined, ["hi"]);
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
        let note_cols = strings(&ctx, &sql.replace("'session'", "'note'")).await;
        assert_eq!(note_cols.first().map(String::as_str), Some("id"));
        assert_eq!(note_cols.last().map(String::as_str), Some("scan"));
    }
}
