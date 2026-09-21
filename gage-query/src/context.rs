use std::sync::Arc;
use std::time::Duration;

use datafusion::datasource::TableProvider;
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion::sql::TableReference;
use datafusion_table_providers::sql::db_connection_pool::Mode;
use datafusion_table_providers::sql::db_connection_pool::sqlitepool::SqliteConnectionPoolFactory;
use datafusion_table_providers::sqlite::SqliteTableFactory;
use gage_claude::driver::ClaudeSource;
use gage_claude::home::ClaudeHome;
use gage_claude::index::IndexStore;
use gage_claude::tables::SessionCache;
use gage_session::{DriverError, Source};

use crate::scope::{Scope, ScopeEdge, ScopedTable, SessionScope};
use crate::tables::config::ConfigTable;
use crate::tables::issue_report::IssueReportFn;
use crate::tables::message_text::MessageTextFn;
use crate::tables::note_doc::note_doc_table;
use crate::tables::note_message_context::NoteMessageContextFn;
use crate::tables::related_issue::RelatedIssueFn;

/// The claude index store behind `source`: the summary cache and
/// text index the `message_text` and `note_message_context`
/// functions and `gage index` read. The functions are the one place
/// gage-query still depends on the claude driver's internals, so the
/// source must be a claude source.
pub fn index_store(source: &dyn Source) -> Result<Arc<IndexStore>, DriverError> {
    Ok(claude_source(source)?.index_store())
}

fn claude_source(source: &dyn Source) -> Result<&ClaudeSource, DriverError> {
    source
        .as_any()
        .downcast_ref::<ClaudeSource>()
        .ok_or_else(|| {
            DriverError::Other(format!(
                "query context requires a claude source, got {:?}",
                source.source()
            ))
        })
}

/// Build a query context over one source: the driver's `session`,
/// `message`, and `entry` tables and the UDF suite; no Gage state
/// tables.
pub fn create_source_context(source: &dyn Source) -> Result<SessionContext, DriverError> {
    let tables = source.tables()?;
    let ctx = new_session_context();
    ctx.register_table("session", tables.session).unwrap();
    ctx.register_table("message", tables.message).unwrap();
    ctx.register_table("entry", tables.entry).unwrap();
    Ok(ctx)
}

/// What an agent context is scoped to: a scan, optionally narrowed to
/// explicit sessions with per-session line ranges.
#[derive(Clone, Debug)]
pub struct AgentScope {
    pub scan_id: String,
    /// When set, the visible session set is exactly this list (instead
    /// of every session in the scan), and each entry's line range
    /// constrains its session's `entry`/`message` rows.
    pub sessions: Option<Vec<SessionScope>>,
}

/// Per-agent context scoped to one `scan_id`. Every readable table
/// (`session`, `entry`, `message`, `note`, `session_note`, `issue`,
/// `issue_evidence`, `session_issue`) returns only rows reachable from that scan's
/// `scan_session` / `scan_note` / `scan_issue` edges. The
/// session-serving TVFs (`message_text`, `note_message_context`) honor
/// the same session scope. The unscoped metadata tables (`config`,
/// `note_doc`) are exposed as they are in the default context.
pub async fn create_agent_context(
    source: &dyn Source,
    scan_id: impl Into<String>,
) -> Result<SessionContext, DriverError> {
    create_agent_context_scoped(
        source,
        AgentScope {
            scan_id: scan_id.into(),
            sessions: None,
        },
    )
    .await
}

/// [`create_agent_context`] with the full [`AgentScope`], including the
/// optional session narrowing and line ranges.
pub async fn create_agent_context_scoped(
    source: &dyn Source,
    scope: AgentScope,
) -> Result<SessionContext, DriverError> {
    build_context(source, Some(scope)).await
}

/// Register the gage JSON UDF suite on a context. Used by
/// `create_context` and by contexts built elsewhere (e.g. gage-scan's
/// per-session scanner context).
pub fn install_udfs(ctx: &SessionContext) {
    let mut ctx_clone = ctx.clone();
    datafusion_functions_json::register_all(&mut ctx_clone).unwrap();
    ctx.register_udf(crate::udf::resolve_ref_udf());
}

/// Build the full query context over an opened source: the driver's
/// session tables, the session-serving functions, and the Gage state
/// tables. The caller resolves and opens the source; this function
/// makes no choice about it.
pub async fn create_context(source: &dyn Source) -> Result<SessionContext, DriverError> {
    build_context(source, None).await
}

async fn build_context(
    source: &dyn Source,
    agent: Option<AgentScope>,
) -> Result<SessionContext, DriverError> {
    // The sqlite connection pool below opens the db file directly and
    // neither creates nor migrates it; a fresh gage home needs both.
    gage_db::db::ensure_db().expect("ensure gage db");
    let ctx = new_session_context();
    let claude = claude_source(source)?;
    let store = claude.index_store();
    let tables = source.tables()?;

    // One session scope shared by the session-serving tables and TVFs
    let session_scope = agent.as_ref().map(|a| {
        Scope::resolve_sessions(&a.scan_id, ScopeEdge::Session, a.sessions.as_deref())
            .expect("resolve scope")
    });

    let mut message_text = MessageTextFn::new(Arc::clone(&store));
    let mut note_context = NoteMessageContextFn::new(Arc::clone(&store));
    if let Some(scope) = &session_scope {
        message_text = message_text.with_scope(scope.clone());
        note_context = note_context.with_scope(scope.clone());
    }
    ctx.register_udtf("message_text", Arc::new(message_text));
    ctx.register_udtf("note_message_context", Arc::new(note_context));
    ctx.register_udtf("issue_report", Arc::new(IssueReportFn::new()));
    ctx.register_udtf("related_issue", Arc::new(RelatedIssueFn::new()));

    register_disk_table(&ctx, "session", "id", None, &session_scope, tables.session);
    register_disk_table(
        &ctx,
        "entry",
        "session_id",
        Some("line"),
        &session_scope,
        tables.entry,
    );
    register_disk_table(
        &ctx,
        "message",
        "session_id",
        Some("line"),
        &session_scope,
        tables.message,
    );

    register_sqlite_tables(
        &ctx,
        agent.as_ref().map(|a| a.scan_id.as_str()),
        &session_scope,
    )
    .await;

    // The `config` table reads the Claude home the source sits under.
    // The env-resolved home carries the real `.claude.json` location
    // ($HOME/.claude.json, a sibling of $HOME/.claude); any other root
    // keeps the fixture layout (`<root>/.claude.json`).
    let claude_home = match ClaudeHome::from_env() {
        Ok(h) if h.path() == claude.root() => h,
        Ok(_) | Err(_) => ClaudeHome::new(claude.root().to_path_buf()),
    };
    ctx.register_table("config", Arc::new(ConfigTable::new(claude_home)))
        .unwrap();
    ctx.register_table("note_doc", note_doc_table().unwrap())
        .unwrap();

    Ok(ctx)
}

/// A bare context with the session cache extension, the PostgreSQL
/// dialect, and the UDF suite installed. No tables.
fn new_session_context() -> SessionContext {
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

/// Register a disk-backed provider, wrapping in [`ScopedTable`] when
/// the context is agent-scoped. `id_col` is the column the wrapper
/// filters on; `line_col` declares the table's line column so the
/// scope's line ranges apply.
fn register_disk_table(
    ctx: &SessionContext,
    name: &str,
    id_col: &'static str,
    line_col: Option<&'static str>,
    session_scope: &Option<Scope>,
    inner: Arc<dyn TableProvider>,
) {
    let provider: Arc<dyn TableProvider> = match session_scope {
        Some(scope) => {
            let mut scoped = ScopedTable::new(inner, id_col, scope.clone());
            if let Some(line_col) = line_col {
                scoped = scoped.with_line_col(line_col);
            }
            Arc::new(scoped)
        }
        None => inner,
    };
    ctx.register_table(name, provider).unwrap();
}

/// Register the sqlite-backed tables ([`SCOPED_SQLITE_TABLES`]) via
/// `SqliteTableFactory`. Each provider uses the
/// standard DataFusion pushdown surface — filters, projection, and
/// limit reach sqlite as `WHERE` / `SELECT col…` / `LIMIT` in the
/// per-scan SQL. When `agent_scan_id` is `Some`, each provider is
/// wrapped in [`ScopedTable`] with the matching `scan_xxx` edge; the
/// wrapper prepends `id IN (…)` to every scan and the sqlite provider
/// unparses it into the pushed-down SQL alongside any caller filters.
/// Session-edge tables reuse `session_scope` — the same (possibly
/// session-narrowed) scope the disk tables filter by.
async fn register_sqlite_tables(
    ctx: &SessionContext,
    agent_scan_id: Option<&str>,
    session_scope: &Option<Scope>,
) {
    let factory = SqliteTableFactory::new(Arc::new(
        SqliteConnectionPoolFactory::new(
            gage_db::db::db_path().to_string_lossy().as_ref(),
            Mode::File,
            Duration::from_secs(5),
        )
        .build()
        .await
        .expect("sqlite connection pool"),
    ));
    for (name, id_col, edge) in SCOPED_SQLITE_TABLES {
        let inner = factory
            .table_provider(TableReference::bare(*name))
            .await
            .unwrap_or_else(|e| panic!("sqlite table provider for {name}: {e}"));
        let provider: Arc<dyn TableProvider> = match agent_scan_id {
            Some(scan_id) => {
                let scope = match edge {
                    ScopeEdge::Session => session_scope
                        .clone()
                        .expect("agent-scoped context should carry a session scope"),
                    ScopeEdge::Note | ScopeEdge::Issue => {
                        Scope::resolve(scan_id, *edge).expect("resolve scope")
                    }
                };
                Arc::new(ScopedTable::new(inner, id_col, scope))
            }
            None => inner,
        };
        ctx.register_table(*name, provider).unwrap();
    }
}

/// The sqlite-backed tables, with the column used for scope filtering
/// and the `scan_xxx` edge that supplies the in-scope id set.
const SCOPED_SQLITE_TABLES: &[(&str, &str, ScopeEdge)] = &[
    ("scan_session", "session_id", ScopeEdge::Session),
    ("note", "id", ScopeEdge::Note),
    ("session_note", "note_id", ScopeEdge::Note),
    ("scan_note", "note_id", ScopeEdge::Note),
    ("issue", "id", ScopeEdge::Issue),
    ("issue_evidence", "issue_id", ScopeEdge::Issue),
    ("session_issue", "issue_id", ScopeEdge::Issue),
    ("scan_issue", "issue_id", ScopeEdge::Issue),
];
