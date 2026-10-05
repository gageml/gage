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
//! [`gage_store::StoredNoteTable`]. `issue` is one row per live issue
//! object and `issue_event` one row per change entry across them.
//! `scan` is one row per live scan object, `scan_task` one row per
//! task of those scans, and `scan_task_agent` one row per agent a
//! task ran.
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

use datafusion::datasource::{MemTable, TableProvider, ViewTable};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_query::SessionCache;
use gage_registry::scanner::ScannerRegistry;
use gage_store::{
    LinkKind, NoteDocRow, Store, StoredNoteTable, StoredSessionTable, attachment_file_table,
    attachment_table, dataset_table, issue_event_table, issue_table, link_table, note_doc_rows,
    note_doc_schema, scan_scope_tables, scan_table, scan_task_agent_table, scan_task_table,
    scan_watermark_table, tag_table,
};

use crate::native::{NativeTable, NativeTableFn};
use crate::project::project_for_path_udf;
use crate::rows::RowCache;
pub use crate::scope::ScanScope;
use crate::scope::SessionSet;
use crate::stored_rows::{RowKind, StoredRowsTable};
use crate::system_cols::SkipSystemCols;

/// Composes a query context. `store` backs the store tables when
/// present; without it, they are absent and only the native discovery
/// functions are available. A store-backed context has store scope,
/// every live object, unless a [`ScanScope`] narrows it to one scan.
pub struct ContextBuilder {
    store: Option<Arc<Mutex<Store>>>,
    scope: Option<ScanScope>,
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

    /// Scope the context to one scan: every table serves that scan's
    /// objects and nothing else. `tag`, `scan_watermark`, and the
    /// `_link` tables are absent, since none is part of one scan's
    /// record.
    pub fn scope(mut self, scope: ScanScope) -> Self {
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

    /// Build the context. A store-scoped context registers the base
    /// tables, the `_link` tables, and the views over them; with
    /// `skip_system_cols` the `_link` tables are left out and the
    /// base tables lose their system columns, while the views keep
    /// reading the link tables they were planned over. A scan-scoped
    /// context registers the scan's tables, with the relations as
    /// tables rather than views.
    pub async fn build(self) -> SessionContext {
        let ctx = new_context(self.skip_system_cols);
        let Some(store) = self.store else {
            return ctx;
        };
        if let Some(scope) = self.scope {
            return build_scan_scope(ctx, store, scope, self.skip_system_cols);
        }
        let sessions = Arc::new(SessionSet::all(Arc::clone(&store)));
        let base: Vec<(&str, Arc<dyn TableProvider>)> = vec![
            (
                "session",
                Arc::new(StoredSessionTable::new(Arc::clone(&store))),
            ),
            (
                "entry",
                Arc::new(StoredRowsTable::new(RowKind::Entry, Arc::clone(&sessions))),
            ),
            (
                "message",
                Arc::new(StoredRowsTable::new(RowKind::Message, sessions)),
            ),
            ("note", Arc::new(StoredNoteTable::new(Arc::clone(&store)))),
            ("dataset", dataset_table(Arc::clone(&store))),
            ("attachment", attachment_table(Arc::clone(&store))),
            ("attachment_file", attachment_file_table(Arc::clone(&store))),
            ("scan", scan_table(Arc::clone(&store))),
            ("scan_task", scan_task_table(Arc::clone(&store))),
            ("scan_task_agent", scan_task_agent_table(Arc::clone(&store))),
            ("issue", issue_table(Arc::clone(&store))),
            ("issue_event", issue_event_table(Arc::clone(&store))),
            ("tag", tag_table(Arc::clone(&store))),
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
        ctx.register_table("note_doc", registry_note_doc_table())
            .expect("register the note doc table on a fresh context");
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

/// Register one scan's tables: `session` at the members' linked
/// versions, `entry` and `message` over them, and every other table
/// from the scan source.
fn build_scan_scope(
    ctx: SessionContext,
    store: Arc<Mutex<Store>>,
    scope: ScanScope,
    skip_system_cols: bool,
) -> SessionContext {
    let members = {
        let guard = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        scope
            .members(&guard)
            .expect("the scan's dataset and members are readable")
    };
    let sessions = Arc::new(SessionSet::fixed(Arc::clone(&store), members));
    let versions = sessions
        .fixed_versions()
        .expect("a scan scope's session set is fixed");
    let mut tables: Vec<(&str, Arc<dyn TableProvider>)> = vec![
        (
            "session",
            Arc::new(StoredSessionTable::at_versions(
                Arc::clone(&store),
                versions,
            )),
        ),
        (
            "entry",
            Arc::new(StoredRowsTable::new(RowKind::Entry, Arc::clone(&sessions))),
        ),
        (
            "message",
            Arc::new(StoredRowsTable::new(RowKind::Message, sessions)),
        ),
    ];
    tables.extend(scan_scope_tables(&store, scope.source()));
    for (name, table) in tables {
        let table: Arc<dyn TableProvider> = if skip_system_cols {
            Arc::new(SkipSystemCols::new(table))
        } else {
            table
        };
        ctx.register_table(name, table)
            .expect("register scan tables on a fresh context");
    }
    ctx
}

/// The `note_doc` table of the store scope: every `writes` declaration
/// of every registered scanner, read once when the context is built.
fn registry_note_doc_table() -> Arc<dyn TableProvider> {
    let registry = ScannerRegistry::load();
    let mut rows = Vec::new();
    for def in registry.list() {
        for (task, def_task) in &def.tasks {
            for (name, doc) in &def_task.notes.writes {
                rows.push(NoteDocRow {
                    note_name: name.clone(),
                    doc: doc.clone(),
                    written_by: format!("{}:{task}", def.name),
                });
            }
        }
    }
    let batch = note_doc_rows(&rows).expect("note doc rows are plain strings");
    Arc::new(
        MemTable::try_new(note_doc_schema(), vec![vec![batch]])
            .expect("a batch of its own schema is a valid table"),
    )
}

/// The system-tier table of watermarks
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
        "dataset_attachment",
        "SELECT l.dataset_id, l.attachment_num, l.attachment_id \
         FROM dataset_attachment_link l JOIN dataset d ON l.dataset_commit = d.commit",
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
    (
        "scan_issue",
        "SELECT scan_id, issue_id FROM scan_issue_link",
    ),
    (
        "issue_evidence",
        "SELECT issue_id, note_id FROM issue_evidence_link",
    ),
    // An issue reaches a session through the notes it cites; the
    // note's current target is used, so an edited note follows
    (
        "session_issue",
        "SELECT DISTINCT t.target_id AS session_id, e.issue_id \
         FROM issue_evidence_link e \
         JOIN note_target_link t ON t.note_id = e.note_id \
         WHERE t.target_type = 'session'",
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
        DatasetStore, IssueInput, IssueStatus, IssueStore, NoteInput, NoteStore, NoteValue,
        ScanStore, SessionSpec, SessionStore, StatusReason, Store, TagStore,
    };
    use tempfile::TempDir;

    use super::{ContextBuilder, ScanScope};

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
        assert_eq!(
            note_cols.last().map(String::as_str),
            Some("carry_forward_key")
        );
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
        let (dataset_id, dataset_commit, session_id, note_id, issue_id) = {
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
                    carry_forward_key: None,
                })
                .unwrap();
            let issues = IssueStore::from(&*store);
            let issue_id = issues
                .create(IssueInput {
                    name: "user-issue",
                    title: "Something",
                    description: Some("Details"),
                    author: "user:t",
                    status: IssueStatus::Pending,
                    evidence: &[note_id.clone()],
                    key: None,
                })
                .unwrap();
            issues
                .set_status(
                    &issue_id,
                    IssueStatus::Closed,
                    Some(StatusReason::WontFix),
                    "user:t",
                    Some("not now"),
                )
                .unwrap();
            let dataset_commit = datasets.get(&dataset_id).unwrap().commit_sha;
            (dataset_id, dataset_commit, session_id, note_id, issue_id)
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

        assert_eq!(
            strings(
                &ctx,
                "SELECT status || ' ' || status_reason || ' ' || evidence_count FROM issue"
            )
            .await,
            ["closed wontfix 1"]
        );
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT event || ' ' || coalesce(to_status, '-') || ' ' || coalesce(message, '-') \
                     FROM issue_event WHERE issue_id = '{issue_id}' ORDER BY event_id"
                )
            )
            .await,
            ["create pending -", "status closed not now"]
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT note_id FROM issue_evidence WHERE issue_id = '{issue_id}'")
            )
            .await,
            [note_id.clone()]
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT session_id FROM session_issue WHERE issue_id = '{issue_id}'")
            )
            .await,
            [session_id.clone()]
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
            [session_id.clone()]
        );
        assert_eq!(
            strings(
                &user,
                &format!("SELECT session_id FROM session_issue WHERE issue_id = '{issue_id}'")
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

    /// `tag` pairs each tag name with the id of the object it names,
    /// aggregates per object, and hides the tagged commit from a
    /// user-facing context.
    #[tokio::test]
    async fn tag_table_names_objects_and_aggregates_per_object() {
        let (_tmp, store) = open_store();
        let dataset_id = {
            let guard = store.lock().unwrap();
            let datasets = DatasetStore::from(&*guard);
            let id = datasets.create().unwrap();
            let other = datasets.create().unwrap();
            let tags = TagStore::from(&*guard);
            tags.add("zeta", &id, false).unwrap();
            tags.add("alpha", &id, false).unwrap();
            tags.add("other", &other, false).unwrap();
            id
        };

        let user = ContextBuilder::new(Some(store))
            .skip_system_cols()
            .build()
            .await;
        assert_eq!(
            strings(
                &user,
                &format!(
                    "SELECT arrow_cast(string_agg(name, ', ' ORDER BY name), 'Utf8') \
                     FROM tag WHERE id = '{dataset_id}' GROUP BY id"
                )
            )
            .await,
            ["alpha, zeta"]
        );
        assert_eq!(
            strings(&user, "SELECT name FROM tag ORDER BY name").await,
            ["alpha", "other", "zeta"]
        );
        assert!(user.sql("SELECT commit FROM tag").await.is_err());
    }

    /// A scan directory `scans/SCAN1/` whose `scan/` tree holds three
    /// tasks and one agent record, with a plan ordering the tasks
    /// differently from tree order. The root `attrs.json` is absent,
    /// as for an active scan.
    fn write_scan_dir(home: &std::path::Path) -> std::path::PathBuf {
        let root = home.join("scans").join("SCAN1");
        let scan = root.join("scan");
        let write = |path: &str, content: &str| {
            let path = scan.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write(
            "plan.json",
            r#"{"tasks":[
                {"task":"hello:greet","selected":"explicit","after":[],"unmatched":[]},
                {"task":"hello:fail","selected":"group:default","after":[],"unmatched":[]},
                {"task":"hello:after","selected":"required_by:greeting","after":[],"unmatched":[]}
            ]}"#,
        );
        write(
            "tasks/hello/greet/attrs.json",
            r#"{"status":"completed","started":1000,"stopped":1200}"#,
        );
        write(
            "tasks/hello/fail/attrs.json",
            r#"{"status":"failed","started":1200,"stopped":1500}"#,
        );
        write(
            "tasks/hello/after/attrs.json",
            r#"{"status":"skipped","skipped":{"needs":"greeting","upstream":["hello:fail"]}}"#,
        );
        write(
            "tasks/hello/greet/agents/AGENT1/attrs.json",
            r#"{"exit_code":0}"#,
        );
        write("tasks/hello/greet/agents/AGENT1/stderr", "");
        write(
            "tasks/hello/greet/agents/AGENT1/result",
            r#"{"is_error":false,"total_cost_usd":0.5}"#,
        );
        root
    }

    /// `scan_task` lists an active scan's tasks in plan order with
    /// their plan selection, and `scan_task_agent` its agent records
    /// with the result verbatim; neither has a commit.
    #[tokio::test]
    async fn scan_dir_scope_serves_tasks_and_agents() {
        let (tmp, store) = open_store();
        let root = write_scan_dir(tmp.path());
        let ctx = ContextBuilder::new(Some(store))
            .scope(ScanScope::scan_dir(&root))
            .build()
            .await;
        assert_eq!(
            strings(&ctx, "SELECT task FROM scan_task ORDER BY num").await,
            ["greet", "fail", "after"]
        );
        assert_eq!(
            strings(&ctx, "SELECT status FROM scan_task ORDER BY num").await,
            ["completed", "failed", "skipped"]
        );
        assert_eq!(
            strings(&ctx, "SELECT selected FROM scan_task ORDER BY num").await,
            ["explicit", "group:default", "required_by:greeting"]
        );
        assert_eq!(
            strings(
                &ctx,
                "SELECT skipped_upstream FROM scan_task WHERE skipped_needs = 'greeting'"
            )
            .await,
            [r#"["hello:fail"]"#]
        );
        assert_eq!(
            strings(
                &ctx,
                "SELECT scan_id FROM scan_task WHERE scan_commit IS NULL"
            )
            .await,
            ["SCAN1", "SCAN1", "SCAN1"]
        );
        assert_eq!(
            strings(
                &ctx,
                "SELECT result FROM scan_task_agent \
                 WHERE task = 'greet' AND session_id = 'AGENT1' AND exit_code = 0"
            )
            .await,
            [r#"{"is_error":false,"total_cost_usd":0.5}"#]
        );
    }

    /// Once the scan is stored, the store scope lists its tasks and
    /// agents under its commit, and the stored scan scope lists the
    /// same rows.
    #[tokio::test]
    async fn stored_scans_serve_tasks_and_agents_in_both_scopes() {
        let (tmp, store) = open_store();
        let root = write_scan_dir(tmp.path());
        std::fs::write(
            root.join("scan").join("attrs.json"),
            r#"{"runtime":"gage 1","started":1000,"stopped":1500,"canceled":false,"tasks":{"total":3,"completed":1,"failed":1,"skipped":1}}"#,
        )
        .unwrap();
        let commit = {
            let store = store.lock().unwrap();
            ScanStore::from(&*store)
                .create("SCAN1", &root.join("scan"))
                .unwrap()
        };

        let ctx = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
        assert_eq!(
            strings(&ctx, "SELECT task FROM scan_task ORDER BY num").await,
            ["greet", "fail", "after"]
        );
        assert_eq!(
            strings(&ctx, "SELECT DISTINCT scan_commit FROM scan_task").await,
            [commit.clone()]
        );
        assert_eq!(
            strings(&ctx, "SELECT session_id FROM scan_task_agent").await,
            ["AGENT1"]
        );
        // The user-facing context keeps the tables and drops the commit
        let user = ContextBuilder::new(Some(Arc::clone(&store)))
            .skip_system_cols()
            .build()
            .await;
        assert_eq!(
            strings(
                &user,
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = 'scan_task' AND column_name LIKE '%commit'"
            )
            .await,
            Vec::<String>::new()
        );

        let scoped = ContextBuilder::new(Some(store))
            .scope(ScanScope::stored("SCAN1"))
            .build()
            .await;
        assert_eq!(
            strings(&scoped, "SELECT task FROM scan_task ORDER BY num").await,
            ["greet", "fail", "after"]
        );
        assert_eq!(
            strings(&scoped, "SELECT scan_commit FROM scan_task_agent").await,
            [commit]
        );
    }
}
