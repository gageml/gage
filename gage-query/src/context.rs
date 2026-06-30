use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use datafusion::datasource::TableProvider;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion::sql::TableReference;
use datafusion_federation::FederatedTableProviderAdaptor;
use datafusion_federation::sql::{SQLFederationProvider, SQLTableSource};
use datafusion_table_providers::sql::db_connection_pool::Mode;
use datafusion_table_providers::sql::db_connection_pool::dbconnection::get_schema;
use datafusion_table_providers::sql::db_connection_pool::sqlitepool::SqliteConnectionPoolFactory;
use datafusion_table_providers::sqlite::DynSqliteConnectionPool;
use datafusion_table_providers::sqlite::sql_table::SQLiteTable;
use gage_index::IndexStore;

use crate::cache::SessionCache;
use crate::scope::sqlite::ScopedSqliteSource;
use crate::scope::{Scope, ScopeEdge, ScopedTable};
use crate::tables::config::ConfigTable;
use crate::tables::entry::EntryTable;
use crate::tables::issue_report::IssueReportFn;
use crate::tables::message::MessageTable;
use crate::tables::message_text::MessageTextFn;
use crate::tables::note_doc::note_doc_table;
use crate::tables::note_message_context::NoteMessageContextFn;
use crate::tables::session::SessionTable;

fn default_root() -> PathBuf {
    gage_claude::session::projects_dir().expect("CLAUDE_PROJECTS_DIR or HOME must be set")
}

/// Cache location for a given corpus `root`. Namespaced by origin so the
/// two corpora — the user's Claude sessions and the gage agent corpus —
/// never share a summary cache, text index, or reconcile manifest. They
/// are keyed by session id, which collides across origins, and reconcile
/// GCs against a single root walk. There are exactly two origins by
/// definition, so the segment is a literal: `agent` for the gage agent
/// corpus (`<gage_home>/claude`, what `gage session -A` selects),
/// `default` for everything else.
fn default_cache_dir(root: &Path) -> PathBuf {
    let gage_home = gage_core::config::gage_home();
    let origin = if root == gage_home.join("claude") {
        "agent"
    } else {
        "default"
    };
    gage_home.join("cache").join(origin)
}

/// The text-index handle for the default corpus and cache locations
/// — what `gage query`, the MCP server, and `gage index` all share.
pub fn default_index_store() -> IndexStore {
    let root = default_root();
    let cache_dir = default_cache_dir(&root);
    IndexStore::new(root, cache_dir)
}

pub async fn create_context_default() -> SessionContext {
    let root = default_root();
    let cache_dir = default_cache_dir(&root);
    create_context(&root, &cache_dir).await
}

/// Per-agent context scoped to one `scan_id`. Every readable table
/// (`session`, `entry`, `message`, `note`, `session_note`, `issue`,
/// `issue_evidence`) returns only rows reachable from that scan's
/// `scan_session` / `scan_note` / `scan_issue` edges. The unscoped
/// metadata tables (`config`, `note_doc`) and TVFs are exposed as
/// they are in the default context.
pub async fn create_agent_context(scan_id: impl Into<String>) -> SessionContext {
    let root = default_root();
    let cache_dir = default_cache_dir(&root);
    build_context(&root, &cache_dir, Some(scan_id.into())).await
}

/// Register the gage JSON UDF suite on a context. Used by
/// `create_context` and by contexts built elsewhere (e.g. gage-scan's
/// per-session scanner context).
pub fn install_udfs(ctx: &SessionContext) {
    let mut ctx_clone = ctx.clone();
    datafusion_functions_json::register_all(&mut ctx_clone).unwrap();
    ctx.register_udf(crate::udf::resolve_ref_udf());
}

/// Build a query context over the session corpus at `root`, with the
/// text index cached under `cache_dir`. Queries reconcile the index
/// lazily; the per-context session cache parses JSONL on first touch.
pub async fn create_context(root: &Path, cache_dir: &Path) -> SessionContext {
    build_context(root, cache_dir, None).await
}

async fn build_context(
    root: &Path,
    cache_dir: &Path,
    agent_scan_id: Option<String>,
) -> SessionContext {
    let cache = Arc::new(SessionCache::new());
    let config = SessionConfig::new()
        .with_information_schema(true)
        .with_extension(Arc::clone(&cache))
        .set_str("datafusion.sql_parser.dialect", "PostgreSQL");
    // Federation rules + query planner so sqlite-only sub-plans are
    // rewritten into a single SQL query handed to sqlite.
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_default_features()
        .with_optimizer_rules(datafusion_federation::default_optimizer_rules())
        .with_query_planner(Arc::new(datafusion_federation::FederatedQueryPlanner::new()))
        .build();
    let ctx = SessionContext::new_with_state(state);
    install_udfs(&ctx);
    let store = Arc::new(IndexStore::new(root, cache_dir));
    ctx.register_udtf(
        "message_text",
        Arc::new(MessageTextFn::new(Arc::clone(&store))),
    );
    ctx.register_udtf(
        "note_message_context",
        Arc::new(NoteMessageContextFn::new(Arc::clone(&store))),
    );
    ctx.register_udtf("issue_report", Arc::new(IssueReportFn::new()));

    register_disk_table(&ctx, "session", "id", ScopeEdge::Session, &agent_scan_id, || {
        Arc::new(SessionTable::new(store.clone()))
    });
    register_disk_table(
        &ctx,
        "entry",
        "session_id",
        ScopeEdge::Session,
        &agent_scan_id,
        || Arc::new(EntryTable::new(store.clone())),
    );
    register_disk_table(
        &ctx,
        "message",
        "session_id",
        ScopeEdge::Session,
        &agent_scan_id,
        || Arc::new(MessageTable::new(store.clone())),
    );

    register_sqlite_tables(&ctx, agent_scan_id.as_deref()).await;

    // `root` is `<claude_home>/projects`; recover the claude_home dir
    // for the `config` table. Tests that pass a non-standard `root`
    // (e.g. a bare `testdata/` dir) get an unrelated home — fine as
    // long as they don't query `config`.
    let claude_home = root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.to_path_buf());
    ctx.register_table("config", Arc::new(ConfigTable::new(claude_home)))
        .unwrap();
    ctx.register_table("note_doc", note_doc_table().unwrap())
        .unwrap();
    ctx
}

/// Register a disk-backed provider, wrapping in [`ScopedTable`] when
/// the context is agent-scoped. `id_col` is the column the wrapper
/// filters on; `make_inner` constructs the unwrapped provider.
fn register_disk_table(
    ctx: &SessionContext,
    name: &str,
    id_col: &'static str,
    edge: ScopeEdge,
    agent_scan_id: &Option<String>,
    make_inner: impl FnOnce() -> Arc<dyn TableProvider>,
) {
    let inner = make_inner();
    let provider: Arc<dyn TableProvider> = match agent_scan_id {
        Some(scan_id) => Arc::new(ScopedTable::new(inner, id_col, Scope::new(scan_id, edge))),
        None => inner,
    };
    ctx.register_table(name, provider).unwrap();
}

/// Register the sqlite-backed tables (`note`, `session_note`, `issue`,
/// `issue_evidence`) wrapped in `FederatedTableProviderAdaptor` so the
/// `FederationOptimizerRule` rewrites multi-table sqlite sub-plans
/// into a single SQL query executed by sqlite. When `agent_scan_id`
/// is `Some`, each registration uses a [`ScopedSqliteSource`] whose
/// `logical_optimizer` injects the scope predicate into that single
/// SQL query.
async fn register_sqlite_tables(ctx: &SessionContext, agent_scan_id: Option<&str>) {
    let pool = sqlite_pool().await;
    for (name, id_col, edge) in SCOPED_SQLITE_TABLES {
        let provider = match agent_scan_id {
            Some(scan_id) => {
                scoped_federated_sqlite_table(&pool, name, id_col, Scope::new(scan_id, *edge)).await
            }
            None => federated_sqlite_table(&pool, name).await,
        };
        ctx.register_table(*name, provider).unwrap();
    }
}

/// The sqlite-backed tables, with the column used for scope filtering
/// and the `scan_xxx` edge that supplies the in-scope id set.
const SCOPED_SQLITE_TABLES: &[(&str, &'static str, ScopeEdge)] = &[
    ("note", "id", ScopeEdge::Note),
    ("session_note", "note_id", ScopeEdge::Note),
    ("issue", "id", ScopeEdge::Issue),
    ("issue_evidence", "issue_id", ScopeEdge::Issue),
];

async fn sqlite_pool() -> Arc<DynSqliteConnectionPool> {
    Arc::new(
        SqliteConnectionPoolFactory::new(
            gage_db::db::db_path().to_string_lossy().as_ref(),
            Mode::File,
            Duration::from_secs(5),
        )
        .build()
        .await
        .expect("sqlite connection pool"),
    )
}

async fn fetch_sqlite_schema(
    pool: &Arc<DynSqliteConnectionPool>,
    table_ref: &TableReference,
) -> datafusion::arrow::datatypes::SchemaRef {
    let name = table_ref.table();
    let conn = pool
        .connect()
        .await
        .unwrap_or_else(|e| panic!("sqlite connect for {name}: {e}"));
    get_schema(conn, table_ref)
        .await
        .unwrap_or_else(|e| panic!("sqlite schema for {name}: {e}"))
}

async fn federated_sqlite_table(
    pool: &Arc<DynSqliteConnectionPool>,
    name: &str,
) -> Arc<dyn TableProvider> {
    let table_ref = TableReference::bare(name);
    let schema = fetch_sqlite_schema(pool, &table_ref).await;
    let read_provider = Arc::new(SQLiteTable::new_with_schema(pool, schema, table_ref));
    Arc::new(
        read_provider
            .create_federated_table_provider()
            .unwrap_or_else(|e| panic!("federated provider for {name}: {e}")),
    )
}

/// Build a federated provider whose `SQLTable` impl injects the
/// scope predicate at federation rewrite time. Federation produces
/// the SQL with the scope filter included; the wrapped
/// [`ScopedTable`] fallback applies the same predicate at `scan()`
/// time when federation declines to rewrite the sub-plan.
async fn scoped_federated_sqlite_table(
    pool: &Arc<DynSqliteConnectionPool>,
    name: &str,
    id_col: &'static str,
    scope: Scope,
) -> Arc<dyn TableProvider> {
    let table_ref = TableReference::bare(name);
    let schema = fetch_sqlite_schema(pool, &table_ref).await;
    let sqlite_table = Arc::new(SQLiteTable::new_with_schema(
        pool,
        schema.clone(),
        table_ref.clone(),
    ));
    let fed_provider = Arc::new(SQLFederationProvider::new(sqlite_table.clone()));
    let scoped_source = Arc::new(ScopedSqliteSource::new(
        table_ref,
        schema,
        id_col,
        scope.clone(),
    ));
    let table_source = Arc::new(SQLTableSource::new_with_table(fed_provider, scoped_source));
    let fallback: Arc<dyn TableProvider> = Arc::new(ScopedTable::new(sqlite_table, id_col, scope));
    Arc::new(FederatedTableProviderAdaptor::new_with_provider(
        table_source,
        fallback,
    ))
}
