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

use datafusion::datasource::{TableProvider, ViewTable};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_query::SessionCache;
use gage_store::{
    LinkKind, Store, StoredNoteTable, StoredSessionTable, dataset_table, link_table, scan_table,
    scan_watermark_table,
};

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
    scope: Option<Arc<SessionScope>>,
    skip_system_cols: bool,
}

impl ContextBuilder {
    pub fn new(store: Option<Arc<Mutex<Store>>>) -> Self {
        Self {
            store,
            scope: None,
            skip_system_cols: false,
        }
    }

    /// Serve `entry` and `message` from `scope` instead of every live
    /// session in the store. The `session` table stays store-wide.
    pub fn scope(mut self, scope: Arc<SessionScope>) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Hide the system columns of every session table. The columns
    /// remain in the data; they leave the schema that `SELECT *`,
    /// `DESCRIBE`, and `information_schema` report.
    pub fn skip_system_cols(mut self) -> Self {
        self.skip_system_cols = true;
        self
    }

    /// Build the context. A store-backed context registers the base
    /// tables, the `_link` tables, and the views over them; with
    /// `skip_system_cols` the `_link` tables are left out and the
    /// base tables lose their system columns, while the views keep
    /// reading the link tables they were planned over.
    pub async fn build(self) -> SessionContext {
        let ctx = new_context(self.skip_system_cols);
        let Some(store) = self.store else {
            return ctx;
        };
        let scope = self
            .scope
            .unwrap_or_else(|| Arc::new(SessionScope::new(Arc::clone(&store))));
        let session: Arc<dyn TableProvider> = match scope.fixed_versions() {
            Some(versions) => Arc::new(StoredSessionTable::at_versions(
                Arc::clone(&store),
                versions,
            )),
            None => Arc::new(StoredSessionTable::new(Arc::clone(&store))),
        };
        let base: Vec<(&str, Arc<dyn TableProvider>)> = vec![
            ("session", session),
            (
                "entry",
                Arc::new(StoredRowsTable::new(RowKind::Entry, Arc::clone(&scope))),
            ),
            (
                "message",
                Arc::new(StoredRowsTable::new(RowKind::Message, scope)),
            ),
            ("note", Arc::new(StoredNoteTable::new(Arc::clone(&store)))),
            ("dataset", dataset_table(Arc::clone(&store))),
            ("scan", scan_table(Arc::clone(&store))),
        ];
        for (name, table) in &base {
            ctx.register_table(*name, Arc::clone(table))
                .expect("register store tables on a fresh context");
        }
        for kind in LinkKind::ALL {
            ctx.register_table(kind.table_name(), link_table(Arc::clone(&store), kind))
                .expect("register link tables on a fresh context");
        }
        ctx.register_table(SCAN_WATERMARK, scan_watermark_table(Arc::clone(&store)))
            .expect("register the watermark table on a fresh context");
        // Views are planned over the raw providers, so they survive the
        // user-facing context hiding what they read
        for (name, sql) in VIEWS {
            let plan = ctx
                .state()
                .create_logical_plan(sql)
                .await
                .expect("view SQL is fixed and names registered tables");
            ctx.register_table(*name, Arc::new(ViewTable::new(plan, Some(sql.to_string()))))
                .expect("register views on a fresh context");
        }
        if self.skip_system_cols {
            for (name, table) in base {
                ctx.deregister_table(name)
                    .expect("deregister a table this build registered");
                ctx.register_table(name, Arc::new(SkipSystemCols::new(table)))
                    .expect("register store tables on a fresh context");
            }
            for name in LinkKind::ALL
                .iter()
                .map(|k| k.table_name())
                .chain([SCAN_WATERMARK])
            {
                ctx.deregister_table(name)
                    .expect("deregister a table this build registered");
            }
        }
        ctx
    }
}

/// The system-tier table of watermarks; see watermarks.md
const SCAN_WATERMARK: &str = "scan_watermark";

/// The user-facing relation tables: views over the `_link` tables
/// with the version already chosen, so a join is on ids.
const VIEWS: &[(&str, &str)] = &[
    (
        "dataset_session",
        "SELECT l.dataset_id, l.session_num, l.session_id \
         FROM dataset_session_link l JOIN dataset d ON l.dataset_commit = d.commit",
    ),
    (
        "scan_session",
        "SELECT s.scan_id, m.session_num, m.session_id \
         FROM scan_dataset_link s \
         JOIN dataset_session_link m ON s.dataset_commit = m.dataset_commit",
    ),
    (
        "scan_note",
        "SELECT scan_id, note_id, carried FROM scan_note_link",
    ),
    (
        "session_note",
        "SELECT target_id AS session_id, note_id, lines \
         FROM note_target_link WHERE target_type = 'session'",
    ),
];

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
    use gage_store::{
        DatasetStore, NoteInput, NoteStore, NoteValue, SessionSpec, SessionStore, Store,
    };
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
        let ctx = ContextBuilder::new(Some(store)).build().await;
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

        let ctx = ContextBuilder::new(Some(store)).build().await;
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
        let ctx = ContextBuilder::new(Some(store)).build().await;
        let cols = strings(&ctx, sql).await;
        assert_eq!(
            &cols[cols.len() - 3..],
            ["id_display", "id_prefix", "locator"]
        );

        let (_tmp, store) = open_store();
        let ctx = ContextBuilder::new(Some(store))
            .skip_system_cols()
            .build()
            .await;
        let cols = strings(&ctx, sql).await;
        assert_eq!(cols.first().map(String::as_str), Some("id"));
        assert!(
            !cols
                .iter()
                .any(|c| c == "id_display" || c == "id_prefix" || c == "locator")
        );
        let note_cols = strings(&ctx, &sql.replace("'session'", "'note'")).await;
        assert_eq!(note_cols.first().map(String::as_str), Some("id"));
        assert_eq!(note_cols.last().map(String::as_str), Some("carry_forward"));
    }

    /// The `_link` tables list the store's link files with both
    /// commits; the views over them resolve versions to ids; the
    /// user-facing context has the views and not the link tables or
    /// the commit columns.
    #[tokio::test]
    async fn link_tables_and_views_resolve_relations() {
        let (_tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-555555555555";
        let project_dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver.open_source(&spec).unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let (dataset_id, dataset_commit, session_id, note_id) = {
            let store = store.lock().unwrap();
            let datasets = DatasetStore::from(&*store);
            let dataset_id = datasets.create().unwrap();
            let added = datasets
                .sessions_add(
                    &dataset_id,
                    vec![SessionSpec {
                        driver: driver.as_ref(),
                        session: native.as_mut(),
                    }],
                )
                .unwrap();
            let session_id = added[0].id.clone();
            let url = format!("session:{session_id}#1");
            let note_id = NoteStore::from(&*store)
                .create(NoteInput {
                    name: "n",
                    value: NoteValue::Text("v".into()),
                    author: "user:t",
                    target: Some(&url),
                    metadata: None,
                    carry_forward: None,
                })
                .unwrap();
            let dataset_commit = datasets.get(&dataset_id).unwrap().commit_sha;
            (dataset_id, dataset_commit, session_id, note_id)
        };

        let ctx = ContextBuilder::new(Arc::clone(&store).into()).build().await;
        assert_eq!(
            strings(&ctx, "SELECT dataset_commit FROM dataset_session_link").await,
            [dataset_commit.clone()]
        );
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT session_id FROM dataset_session WHERE dataset_id = '{dataset_id}'"
                )
            )
            .await,
            [session_id.clone()]
        );
        assert_eq!(
            strings(&ctx, "SELECT commit FROM dataset").await,
            [dataset_commit]
        );
        assert_eq!(
            strings(&ctx, "SELECT target_type FROM note_target_link").await,
            ["session"]
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT lines FROM session_note WHERE session_id = '{session_id}' AND note_id = '{note_id}'")
            )
            .await,
            ["1"]
        );

        let user = ContextBuilder::new(Some(store))
            .skip_system_cols()
            .build()
            .await;
        assert_eq!(
            strings(
                &user,
                &format!(
                    "SELECT session_id FROM dataset_session WHERE dataset_id = '{dataset_id}'"
                )
            )
            .await,
            [session_id]
        );
        let tables = strings(
            &user,
            "SELECT table_name FROM information_schema.tables WHERE table_name LIKE '%_link' ORDER BY table_name",
        )
        .await;
        assert!(tables.is_empty(), "{tables:?}");
        assert!(user.sql("SELECT commit FROM dataset").await.is_err());
    }
}
