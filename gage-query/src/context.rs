use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion::sql::TableReference;
use datafusion_table_providers::sql::db_connection_pool::Mode;
use datafusion_table_providers::sql::db_connection_pool::sqlitepool::SqliteConnectionPoolFactory;
use datafusion_table_providers::sqlite::SqliteTableFactory;
use gage_index::IndexStore;

use crate::cache::SessionCache;
use crate::tables::config::ConfigTable;
use crate::tables::entry::EntryTable;
use crate::tables::message::MessageTable;
use crate::tables::message_text::MessageTextFn;
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

/// Register the gage JSON UDF suite on a context. Used by
/// `create_context` and by contexts built elsewhere (e.g. gage-scan's
/// per-session scanner context).
pub fn install_udfs(ctx: &SessionContext) {
    let mut ctx_clone = ctx.clone();
    datafusion_functions_json::register_all(&mut ctx_clone).unwrap();
}

/// Build a query context over the session corpus at `root`, with the
/// text index cached under `cache_dir`. Queries reconcile the index
/// lazily; the per-context session cache parses JSONL on first touch.
pub async fn create_context(root: &Path, cache_dir: &Path) -> SessionContext {
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
    ctx.register_table("session", Arc::new(SessionTable::new(store.clone())))
        .unwrap();
    ctx.register_table("entry", Arc::new(EntryTable::new(store.clone())))
        .unwrap();
    ctx.register_table("message", Arc::new(MessageTable::new(store)))
        .unwrap();
    register_sqlite_tables(&ctx).await;
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
    ctx
}

/// Register the sqlite-backed tables (`note`, `session_note`, `issue`,
/// `issue_evidence`) via `datafusion-table-providers`. The provider
/// introspects the schema from sqlite at registration time and,
/// combined with the `FederationOptimizerRule` installed on the
/// session, rewrites sqlite-only sub-plans into a single SQL query
/// executed by sqlite.
async fn register_sqlite_tables(ctx: &SessionContext) {
    let pool = Arc::new(
        SqliteConnectionPoolFactory::new(
            gage_db::db::db_path().to_string_lossy().as_ref(),
            Mode::File,
            Duration::from_secs(5),
        )
        .build()
        .await
        .expect("sqlite connection pool"),
    );
    let factory = SqliteTableFactory::new(pool);
    for name in ["note", "session_note", "issue", "issue_evidence"] {
        let provider = factory
            .table_provider(TableReference::bare(name))
            .await
            .unwrap_or_else(|e| panic!("sqlite table provider for {name}: {e}"));
        ctx.register_table(name, provider).unwrap();
    }
}
