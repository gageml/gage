//! Composed DataFusion query surface over Gage data.
//!
//! gage-query is the query orchestrator. It owns `SessionContext`
//! creation and the context-level configuration --- the
//! [`rows::RowCache`] extension, the SQL dialect,
//! `information_schema`, and the UDF suite --- and composes what the
//! data crates and drivers contribute.
//!
//! Named tables are store data. `session` is one row per live stored
//! session object, from [`gage_store::StoredSessionTable`]. `entry`,
//! `message`, and `tool_use` are the rows of those
//! sessions: the core resolves the session set and plans every read
//! ([`scope`]), and the driver that wrote each session deserializes
//! its bytes into normalized rows the core turns into batches
//! ([`rows`], [`stored_rows`]). `session_change` is the records of a
//! published session's `changes.json`, over the same session set
//! ([`session_change`]).
//! `note` is one row per live note object, from
//! [`gage_store::StoredNoteTable`]. `issue` is one row per live issue
//! object and `issue_event` one row per change entry across them.
//! `scan` is one row per live scan object, `scan_task` one row per
//! task of those scans, `scan_task_agent` one row per agent a task
//! ran, and `scan_log` one row per log a scan wrote. `dataset` and
//! `share` are one row per live container object, with `dataset_session`
//! and `share_session` their memberships.
//!
//! Native session data is reached through driver-provided functions,
//! present on every context: `native_session`, `native_message`,
//! `native_entry`, and `native_tool_use` ([`native`]) return a
//! source's native tables, and
//! the [`project`] `project_for_path` UDF maps a directory to its
//! project name for filtering. These are how a user discovers
//! sessions to add to the store; none is a named table.
//! `content_schema` is on every context too: each builtin driver's
//! description of what it writes into `raw`, `input_raw`, and
//! `result_raw`, per content format. `help` is on every context as
//! well: one row per usage-note topic that SQL introspection cannot
//! supply, read with `SELECT full FROM help WHERE topic = '<topic>'`.
//!
//! Session tables carry [`system_cols`], the columns a program needs
//! to render or address a row. A context includes them by default;
//! [`ContextBuilder::skip_system_cols`] hides them for a context that
//! serves people and models writing queries.

mod age;
mod blame;
pub mod help;
mod json;
mod native;
mod patch;
mod print_format;
mod project;
mod repl;
pub mod rows;
pub mod scope;
pub mod session_change;
pub mod stored_rows;
pub mod system_cols;
pub mod text;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use datafusion::datasource::{MemTable, TableProvider, ViewTable};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_registry::driver::DriverRegistry;
use gage_registry::scanner::ScannerRegistry;
use gage_session::{ContentSchema, content_schema_batch, content_schema_schema};
use gage_store::{
    ExternalSession, LinkKind, NoteDocRow, Store, StoredNoteTable, StoredSessionTable,
    attachment_file_table, attachment_table, dataset_table, issue_event_table, issue_report_fn,
    issue_table, link_table, note_doc_rows, note_doc_schema, object_key_table, related_issue_fn,
    scan_log_table, scan_scope_tables, scan_table, scan_task_agent_table, scan_task_table,
    scan_watermark_table, share_table, tag_table,
};

use crate::age::age_udf;
use crate::blame::{blame_fn, blame_schema, native_blame_fn};
use crate::help::help_table;
use crate::json::json_extract_udf;
use crate::native::{NativeTable, NativeTableFn};
use crate::patch::patch_lines_udf;
pub use crate::print_format::{PrintFormat, write_yaml, write_yaml_capped};
use crate::project::project_for_path_udf;
pub use crate::repl::{TvfInfo, exec_command, run_repl};
use crate::rows::RowCache;
pub use crate::scope::ScanScope;
use crate::scope::SessionSet;
use crate::stored_rows::{RowKind, StoredRowsTable};
use crate::system_cols::SkipSystemCols;
use crate::text::{
    MESSAGE_TEXT_ARGS, MessageTextFn, NATIVE_MESSAGE_TEXT_ARGS, NativeMessageTextFn,
    StoreTextIndex, message_text_schema,
};

/// Composes a query context. `store` backs the store tables when
/// present; without it, they are absent and only the native discovery
/// functions are available. A store-backed context has store scope,
/// every live object, unless a [`ScanScope`] narrows it to one scan
/// or [`external`](Self::external) narrows it to given session
/// directories.
pub struct ContextBuilder {
    store: Option<Arc<Mutex<Store>>>,
    scope: Option<ScanScope>,
    external: Option<Vec<ExternalSession>>,
    skip_system_cols: bool,
    show_agents: bool,
    cache_root: Option<PathBuf>,
}

impl ContextBuilder {
    pub fn new(store: Option<Arc<Mutex<Store>>>) -> Self {
        Self {
            store,
            scope: None,
            external: None,
            skip_system_cols: false,
            show_agents: false,
            cache_root: None,
        }
    }

    /// Keep the derived state of every source the context opens under
    /// `root`. The native table functions, `native_blame`, `blame`,
    /// and `project_for_path` open sources. Without this, a context
    /// over a store uses the store's cache root, and one without a
    /// store uses the default.
    pub fn cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.cache_root = Some(root.into());
        self
    }

    /// Scope the context to one scan: every table serves that scan's
    /// objects and nothing else. `tag`, `scan_watermark`, and the
    /// `_link` tables are absent, since none is part of one scan's
    /// record.
    pub fn scope(mut self, scope: ScanScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Scope the context to `sessions`, session directories outside
    /// the store: the session tables serve them and nothing else, and
    /// no other store table is present. Each directory is read on
    /// each query. Takes precedence over a scan scope.
    pub fn external(mut self, sessions: Vec<ExternalSession>) -> Self {
        self.external = Some(sessions);
        self
    }

    /// Hide the system columns of every session table. The columns
    /// remain in the data; they leave the schema that `SELECT *`,
    /// `DESCRIBE`, and `information_schema` report.
    pub fn skip_system_cols(mut self) -> Self {
        self.skip_system_cols = true;
        self
    }

    /// Let the native table functions read the runtime's own in-flight
    /// agent sessions, which every other context leaves out. The live
    /// scan view reads a running agent through such a context.
    pub fn show_agents(mut self) -> Self {
        self.show_agents = true;
        self
    }

    /// Build the context. A store-scoped context registers the base
    /// tables, the `_link` tables, and the views over them; with
    /// `skip_system_cols` the `_link` tables are left out and the
    /// base tables lose their system columns, while the views keep
    /// reading the link tables they were planned over. A scan-scoped
    /// context registers the scan's tables, with the relations as
    /// tables rather than views. An external-scoped context registers
    /// the session tables alone.
    pub async fn build(self) -> SessionContext {
        let cache_root = self.cache_root.or_else(|| {
            self.store.as_ref().map(|store| {
                store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .cache_root()
                    .to_path_buf()
            })
        });
        let ctx = new_context(self.skip_system_cols, self.show_agents, cache_root.clone());
        let Some(store) = self.store else {
            register_blame_functions(&ctx, None, self.show_agents, cache_root);
            return ctx;
        };
        if let Some(sessions) = self.external {
            return build_external_scope(ctx, store, sessions, self.skip_system_cols);
        }
        if let Some(scope) = self.scope {
            return build_scan_scope(ctx, store, scope, self.skip_system_cols);
        }
        let sessions = Arc::new(SessionSet::all(Arc::clone(&store)));
        ctx.register_udtf(
            "message_text",
            Arc::new(MessageTextFn::new(Arc::new(StoreTextIndex::new(
                Arc::clone(&store),
                cache_root.clone(),
            )))),
        );
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
                Arc::new(StoredRowsTable::new(
                    RowKind::Message,
                    Arc::clone(&sessions),
                )),
            ),
            (
                "tool_use",
                Arc::new(StoredRowsTable::new(
                    RowKind::ToolUse,
                    Arc::clone(&sessions),
                )),
            ),
            (
                "tool_use_file",
                Arc::new(StoredRowsTable::new(
                    RowKind::ToolUseFile,
                    Arc::clone(&sessions),
                )),
            ),
            (
                "session_change",
                Arc::new(StoredRowsTable::new(RowKind::SessionChange, sessions)),
            ),
            ("note", Arc::new(StoredNoteTable::new(Arc::clone(&store)))),
            ("dataset", dataset_table(Arc::clone(&store))),
            ("share", share_table(Arc::clone(&store))),
            ("attachment", attachment_table(Arc::clone(&store))),
            ("attachment_file", attachment_file_table(Arc::clone(&store))),
            ("scan", scan_table(Arc::clone(&store))),
            ("scan_task", scan_task_table(Arc::clone(&store))),
            ("scan_task_agent", scan_task_agent_table(Arc::clone(&store))),
            ("scan_log", scan_log_table(Arc::clone(&store))),
            ("issue", issue_table(Arc::clone(&store))),
            ("issue_event", issue_event_table(Arc::clone(&store))),
            ("tag", tag_table(Arc::clone(&store))),
            ("object_key", object_key_table(Arc::clone(&store))),
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
        register_issue_functions(&ctx, &store);
        register_blame_functions(&ctx, Some(&store), self.show_agents, cache_root);
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

/// Register the external scope's tables: `session` over the given
/// session directories, and `entry`, `message`, `tool_use`,
/// `tool_use_file`, and `session_change` over the same set. Notes, issues, and the other store
/// tables are absent, since an external session is not a store object
/// and nothing targets it.
fn build_external_scope(
    ctx: SessionContext,
    store: Arc<Mutex<Store>>,
    external: Vec<ExternalSession>,
    skip_system_cols: bool,
) -> SessionContext {
    let sessions = Arc::new(SessionSet::external(Arc::clone(&store), external.clone()));
    let tables: [(&str, Arc<dyn TableProvider>); 6] = [
        (
            "session",
            Arc::new(StoredSessionTable::external(store, external)),
        ),
        (
            "entry",
            Arc::new(StoredRowsTable::new(RowKind::Entry, Arc::clone(&sessions))),
        ),
        (
            "message",
            Arc::new(StoredRowsTable::new(
                RowKind::Message,
                Arc::clone(&sessions),
            )),
        ),
        (
            "tool_use",
            Arc::new(StoredRowsTable::new(
                RowKind::ToolUse,
                Arc::clone(&sessions),
            )),
        ),
        (
            "tool_use_file",
            Arc::new(StoredRowsTable::new(
                RowKind::ToolUseFile,
                Arc::clone(&sessions),
            )),
        ),
        (
            "session_change",
            Arc::new(StoredRowsTable::new(RowKind::SessionChange, sessions)),
        ),
    ];
    for (name, table) in tables {
        let table: Arc<dyn TableProvider> = if skip_system_cols {
            Arc::new(SkipSystemCols::new(table))
        } else {
            table
        };
        ctx.register_table(name, table)
            .expect("register external tables on a fresh context");
    }
    ctx
}

/// Register one scan's tables: `session` at the members' linked
/// versions, `entry`, `message`, `tool_use`, `tool_use_file`, and
/// `session_change` over them, and every other table from the scan
/// source.
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
            Arc::new(StoredRowsTable::new(
                RowKind::Message,
                Arc::clone(&sessions),
            )),
        ),
        (
            "tool_use",
            Arc::new(StoredRowsTable::new(
                RowKind::ToolUse,
                Arc::clone(&sessions),
            )),
        ),
        (
            "tool_use_file",
            Arc::new(StoredRowsTable::new(
                RowKind::ToolUseFile,
                Arc::clone(&sessions),
            )),
        ),
        (
            "session_change",
            Arc::new(StoredRowsTable::new(RowKind::SessionChange, sessions)),
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
    register_issue_functions(&ctx, &store);
    ctx
}

/// The table functions that read one issue from the store by id:
/// `issue_report` and `related_issue`. Both scopes have them, since
/// they read by id rather than over a scope's set.
fn register_issue_functions(ctx: &SessionContext, store: &Arc<Mutex<Store>>) {
    ctx.register_udtf("issue_report", issue_report_fn(Arc::clone(store)));
    ctx.register_udtf("related_issue", related_issue_fn(Arc::clone(store)));
}

/// The blame functions are for a store-wide or a native context. A
/// scan-scoped context does not register them.
fn register_blame_functions(
    ctx: &SessionContext,
    store: Option<&Arc<Mutex<Store>>>,
    show_agents: bool,
    cache_root: Option<PathBuf>,
) {
    ctx.register_udtf(
        "native_blame",
        native_blame_fn(show_agents, cache_root.clone()),
    );
    if let Some(store) = store {
        ctx.register_udtf("blame", blame_fn(Arc::clone(store), cache_root));
    }
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
        "share_session",
        "SELECT l.share_id, l.session_num, l.session_id \
         FROM share_session_link l JOIN share s ON l.share_commit = s.commit",
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
pub fn repl_functions() -> Vec<TvfInfo> {
    NATIVE_TABLES
        .iter()
        .map(|t| TvfInfo {
            name: t.function_name(),
            args: "source text",
            schema: None,
        })
        .chain([
            TvfInfo {
                name: "blame",
                args: "file text [, lines text]",
                schema: Some(blame_schema()),
            },
            TvfInfo {
                name: "native_blame",
                args: "source text, file text [, lines text]",
                schema: Some(blame_schema()),
            },
            TvfInfo {
                name: "message_text",
                args: MESSAGE_TEXT_ARGS,
                schema: Some(message_text_schema()),
            },
            TvfInfo {
                name: "native_message_text",
                args: NATIVE_MESSAGE_TEXT_ARGS,
                schema: Some(message_text_schema()),
            },
        ])
        .collect()
}

const NATIVE_TABLES: [NativeTable; 5] = [
    NativeTable::Session,
    NativeTable::Message,
    NativeTable::Entry,
    NativeTable::ToolUse,
    NativeTable::ToolUseFile,
];

/// A context carrying the shared configuration --- the `RowCache`
/// extension, the PostgreSQL SQL dialect, `information_schema`, the
/// native table functions, the JSON functions, the `project_for_path`
/// UDF, and the `content_schema` and `help` tables --- with no store
/// tables registered. Sources the functions open keep their derived
/// state under `cache_root`, or the default root when it is `None`.
pub(crate) fn new_context(
    skip_system_cols: bool,
    show_agents: bool,
    cache_root: Option<PathBuf>,
) -> SessionContext {
    let config = SessionConfig::new()
        .with_information_schema(true)
        .with_extension(Arc::new(RowCache::new()))
        .set_str("datafusion.sql_parser.dialect", "PostgreSQL");
    let state = SessionStateBuilder::new()
        .with_config(config)
        .with_default_features()
        .build();
    let mut ctx = SessionContext::new_with_state(state);
    datafusion_functions_json::register_all(&mut ctx).unwrap();
    ctx.register_udf(json_extract_udf());
    for table in NATIVE_TABLES {
        ctx.register_udtf(
            table.function_name(),
            Arc::new(NativeTableFn {
                table,
                skip_system_cols,
                show_agents,
                cache_root: cache_root.clone(),
            }),
        );
    }
    ctx.register_udtf(
        "native_message_text",
        Arc::new(NativeMessageTextFn {
            show_agents,
            cache_root: cache_root.clone(),
        }),
    );
    ctx.register_udf(project_for_path_udf(cache_root));
    ctx.register_udf(age_udf());
    ctx.register_udf(patch_lines_udf());
    ctx.register_table("content_schema", content_schema_table())
        .unwrap();
    ctx.register_table("help", help_table()).unwrap();
    ctx
}

/// Every builtin driver's content schemas as one table
fn content_schema_table() -> Arc<dyn TableProvider> {
    let rows: Vec<ContentSchema> = DriverRegistry::builtin()
        .drivers()
        .iter()
        .flat_map(|d| d.content_schemas())
        .collect();
    let batch = content_schema_batch(&rows).expect("content schema rows are plain strings");
    Arc::new(
        MemTable::try_new(content_schema_schema(), vec![vec![batch]])
            .expect("content schema batch matches its schema"),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::slice;
    use std::sync::{Arc, Mutex};

    use datafusion::arrow::array::StringArray;
    use datafusion::arrow::compute::cast;
    use datafusion::arrow::datatypes::DataType;
    use datafusion::prelude::SessionContext;
    use gage_core::uuid::new_uuid;
    use gage_registry::driver::DriverRegistry;
    use gage_session::SourceOptions;
    use gage_store::{
        DatasetStore, IssueInput, IssueStatus, IssueStore, NoteInput, NoteStore, NoteValue,
        PublishAttrs, PublishFilter, ScanStore, SessionAttrsRecord, SessionDirLayout, SessionSpec,
        SessionStore, ShareAttrs, ShareStore, StatusReason, Store, TagStore,
    };
    use tempfile::TempDir;

    use super::help::help_topics;
    use super::{ContextBuilder, ScanScope};

    /// A session directory named by `id` under `root`: `attrs.json`,
    /// `changes.json`, and `files.d/` under `session/`.
    fn write_session_dir(
        root: &Path,
        id: &str,
        attrs: &SessionAttrsRecord,
        changes: &str,
        files: &[(&str, &str)],
    ) -> PathBuf {
        let dir = root.join(id);
        let layout = SessionDirLayout::new(&dir);
        std::fs::create_dir_all(layout.files_dir()).unwrap();
        let value = serde_json::to_value(attrs).unwrap();
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        std::fs::write(layout.attrs_file(), bytes).unwrap();
        std::fs::write(layout.changes_file(), changes).unwrap();
        for (path, text) in files {
            std::fs::write(layout.files_dir().join(path), text).unwrap();
        }
        dir
    }

    /// A fresh store and the guard keeping its directory alive
    fn open_store() -> (TempDir, Arc<Mutex<Store>>) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Store::open_with_cache_root(&path, &tmp.path().join("cache")).unwrap();
        (tmp, Arc::new(Mutex::new(store)))
    }

    /// A context over a store, built with no cache root of its own,
    /// keeps every index under the store's cache root rather than the
    /// default one.
    #[tokio::test]
    async fn a_store_context_defaults_to_the_stores_cache_root() {
        let (tmp, store) = open_store();
        let ctx = ContextBuilder::new(Some(store)).build().await;
        assert!(
            strings(&ctx, "SELECT session_id FROM message_text('quokka')")
                .await
                .is_empty()
        );
        let corpus = gage_core::cache::store_corpus_dir(
            &tmp.path().join("cache"),
            &tmp.path().join("store.git"),
        );
        assert!(corpus.join("text").is_dir(), "{}", corpus.display());
    }

    struct Explained {
        physical: String,
    }

    /// The physical plan text of `EXPLAIN sql`.
    async fn explain(ctx: &SessionContext, sql: &str) -> Explained {
        let batches = ctx
            .sql(&format!("EXPLAIN {sql}"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let mut physical = String::new();
        for b in &batches {
            let kinds = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
            let plans = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
            for i in 0..b.num_rows() {
                if kinds.value(i) == "physical_plan" {
                    physical.push_str(plans.value(i));
                }
            }
        }
        Explained { physical }
    }

    /// The first column of every row as strings. A string column of
    /// any width is read; a cast result is `Utf8View` under the
    /// PostgreSQL dialect's string mapping.
    async fn strings(ctx: &SessionContext, sql: &str) -> Vec<String> {
        let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
        batches
            .iter()
            .flat_map(|b| {
                let col = cast(b.column(0), &DataType::Utf8).unwrap();
                let col = col.as_any().downcast_ref::<StringArray>().unwrap();
                (0..b.num_rows())
                    .map(|i| col.value(i).to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// An issue with no evidence, for the issue function tests
    fn add_issue(store: &Store, name: &str, title: &str, description: &str) -> String {
        IssueStore::from(store)
            .create(IssueInput {
                name,
                title,
                description: Some(description),
                author: "user:t",
                status: IssueStatus::Pending,
                evidence: &[],
                key: None,
            })
            .unwrap()
    }

    /// `age` renders the time since a timestamp column as the CLI
    /// does.
    #[tokio::test]
    async fn age_renders_time_since_a_timestamp() {
        let (_tmp, store) = open_store();
        {
            let store = store.lock().unwrap();
            add_issue(&store, "general", "Something", "Details.");
        }
        let ctx = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
        let ages = strings(&ctx, "SELECT age(created) FROM issue").await;
        assert_eq!(ages.len(), 1);
        assert!(ages[0].ends_with("s ago"), "{ages:?}");
    }

    /// `related_issue` ranks the docket by TF-IDF cosine over title
    /// and description, leaves the subject and issues under the score
    /// floor out, discounts an issue of another name, and honors a
    /// floor given as the second argument.
    #[tokio::test]
    async fn related_issue_ranks_the_docket_against_the_subject() {
        let (_tmp, store) = open_store();
        let (subject, same, other_name) = {
            let store = store.lock().unwrap();
            let subject = add_issue(
                &store,
                "general",
                "Retry loop never terminates in fetch",
                "The fetch retry loop has no exit.",
            );
            let same = add_issue(
                &store,
                "general",
                "Fetch retry loop runs forever",
                "The retry loop in fetch never exits.",
            );
            let other_name = add_issue(
                &store,
                "code-review",
                "Fetch retry loop runs forever",
                "The retry loop in fetch never exits.",
            );
            add_issue(
                &store,
                "general",
                "Fetch help text is stale",
                "The fetch command help names an option that was removed.",
            );
            add_issue(
                &store,
                "general",
                "Missing parser coverage",
                "Parser lacks tests.",
            );
            (subject, same, other_name)
        };
        let ctx = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
        let sql = format!(
            "SELECT arrow_cast(related_id || ' ' || CAST(round(score, 3) AS VARCHAR), 'Utf8') \
             FROM related_issue('{subject}')"
        );
        let rows = strings(&ctx, &sql).await;
        let (first, first_score) = rows[0].split_once(' ').unwrap();
        let (second, second_score) = rows[1].split_once(' ').unwrap();
        assert_eq!((first, second), (same.as_str(), other_name.as_str()));
        let (first_score, second_score): (f64, f64) =
            (first_score.parse().unwrap(), second_score.parse().unwrap());
        assert!(
            (second_score - first_score * 0.8).abs() < 0.002,
            "another name keeps 80%: {rows:?}"
        );
        assert_eq!(
            rows.len(),
            2,
            "the help issue shares one term and the parser issue none: {rows:?}"
        );
        let floor = (first_score + second_score) / 2.0;
        let raised = strings(
            &ctx,
            &format!("SELECT related_id FROM related_issue('{subject}', {floor})"),
        )
        .await;
        assert_eq!(raised, [same], "a floor between the two scores keeps one");
        let unknown = strings(&ctx, "SELECT related_id FROM related_issue('nope')").await;
        assert!(unknown.is_empty());
    }

    /// `issue_report` renders the issue's fields, description,
    /// evidence at the cited commit, and history, with a duplicate
    /// close naming its survivor.
    #[tokio::test]
    async fn issue_report_renders_fields_evidence_and_history() {
        let (_tmp, store) = open_store();
        let (subject, survivor) = {
            let store = store.lock().unwrap();
            let note_id = NoteStore::from(&*store)
                .create(NoteInput {
                    name: "finding",
                    value: NoteValue::Text("the evidence".into()),
                    author: "user:t",
                    target: None,
                    metadata: None,
                    work_id: None,
                })
                .unwrap();
            let issues = IssueStore::from(&*store);
            let subject = issues
                .create(IssueInput {
                    name: "general",
                    title: "Retry loop never terminates",
                    description: Some("## Summary\n\nIt spins.\n"),
                    author: "task:general:main",
                    status: IssueStatus::Pending,
                    evidence: slice::from_ref(&note_id),
                    key: None,
                })
                .unwrap();
            let survivor = add_issue(&store, "general", "Retry loop spins", "Spins.");
            issues
                .close_duplicate(&subject, &survivor, "user:t", Some("same condition"))
                .unwrap();
            (subject, survivor)
        };
        let ctx = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
        let rows = strings(
            &ctx,
            &format!("SELECT report FROM issue_report('{subject}')"),
        )
        .await;
        let report = &rows[0];
        for expected in [
            "**Retry loop never terminates**",
            "- status: closed",
            "- closed reason: duplicate",
            "- author: task:general:main",
            "## Summary\n\nIt spins.",
            "## Evidence",
            "### finding (note ID ",
            "- value: the evidence",
            "## History",
            "- created pending · task:general:main",
            &format!("- pending → closed (duplicate of {survivor}) · user:t"),
            "  > same condition",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }
        let unknown = strings(&ctx, "SELECT report FROM issue_report('nope')").await;
        assert!(unknown.is_empty());
        let events = strings(
            &ctx,
            &format!(
                "SELECT duplicate_of FROM issue_event WHERE issue_id = '{subject}' AND event = 'status'"
            ),
        )
        .await;
        assert_eq!(events, [survivor]);
    }

    /// `native_blame` is on every context outside a scan scope; `blame`
    /// needs the store.
    #[tokio::test]
    async fn blame_functions_follow_the_context_scope() {
        let ctx = ContextBuilder::new(None).build().await;
        assert!(ctx.table_function("native_blame").is_ok());
        assert!(ctx.table_function("blame").is_err());

        let (_tmp, store) = open_store();
        let ctx = ContextBuilder::new(Some(store)).build().await;
        assert!(ctx.table_function("native_blame").is_ok());
        assert!(ctx.table_function("blame").is_ok());
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
    /// `project` and `mtime` predicates and the limit reach the
    /// driver's walk: the plan carries no filter of its own, and the
    /// rows are the walk's answer.
    #[tokio::test]
    async fn content_schema_lists_every_builtin_driver_format() {
        let ctx = ContextBuilder::new(None).build().await;
        let rows = strings(
            &ctx,
            "SELECT content_format || ' ' || name FROM content_schema ORDER BY 1",
        )
        .await;
        assert_eq!(
            rows,
            [
                "claude-jsonl 1 entry",
                "claude-jsonl 1 tool_use_input",
                "claude-jsonl 1 tool_use_result",
            ]
        );
        let schema = strings(
            &ctx,
            "SELECT schema FROM content_schema WHERE name = 'entry'",
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(&schema[0]).unwrap();
        assert_eq!(parsed["type"], "object");
    }

    /// `help` is queryable on every context, one row per usage-note
    /// topic, each with a non-empty short and full.
    #[tokio::test]
    async fn help_serves_one_row_per_topic() {
        let ctx = ContextBuilder::new(None).build().await;
        let topics = strings(&ctx, "SELECT topic FROM help ORDER BY topic").await;
        let mut expected: Vec<String> = help_topics().iter().map(|t| t.topic.to_string()).collect();
        expected.sort();
        assert_eq!(topics, expected);
        let empty = strings(&ctx, "SELECT topic FROM help WHERE full = '' OR short = ''").await;
        assert!(empty.is_empty(), "{empty:?}");
    }

    /// `native_tool_use_file` lists one row per call per path it
    /// names: a judged write is `write`, and a path a shell command
    /// names with no recorded outcome is `cite`, resolved against the
    /// entry's cwd. The path lists are gone from `native_session`.
    #[tokio::test]
    async fn native_tool_use_file_lists_the_paths_each_call_names() {
        let claude_root = tempfile::tempdir().unwrap();
        let dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "cccccccc-0000-0000-0000-000000000001";
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            concat!(
                r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","cwd":"/w/proj","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/proj/a.rs","old_string":"x","new_string":"y"}},{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cat > /etc/x/cfg.json <<EOF\n{}\nEOF\ncat b/c.rs"}}]}}"#,
                "\n",
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let from = format!("native_tool_use_file('{spec}')");
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT tool_use_id || ' ' || type || ' ' || path FROM {from} \
                     ORDER BY tool_use_id, path"
                )
            )
            .await,
            [
                "t1 write /w/proj/a.rs",
                "t2 cite /etc/x/cfg.json",
                "t2 cite /w/proj/b/c.rs",
            ]
        );
        assert!(
            claude_root.path().join("cache").is_dir(),
            "the source's cache is under the context's root"
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT DISTINCT session_id FROM {from} WHERE path = '/etc/x/cfg.json'")
            )
            .await,
            [id]
        );
        let err = ctx
            .sql(&format!(
                "SELECT files_changed FROM native_session('{spec}')"
            ))
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("files_changed"), "{err}");
    }

    /// `native_session` carries the driver's normalized attributes as
    /// typed columns and again in the same `summary` JSON the stored
    /// `session` table carries, with the driver's extra members beside
    /// them.
    #[tokio::test]
    async fn native_session_carries_the_summary_json() {
        let claude_root = tempfile::tempdir().unwrap();
        let dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "dddddddd-0000-0000-0000-000000000001";
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"Fix the parser"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":3,"output_tokens":2,"cache_read_input_tokens":1}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let empty = "dddddddd-0000-0000-0000-000000000002";
        std::fs::write(dir.join(format!("{empty}.jsonl")), "").unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let from = format!("native_session('{spec}')");
        let cols = strings(
            &ctx,
            &format!(
                "SELECT json_extract(summary, '$.title') || '|' || \
                        json_extract(summary, '$.model') || '|' || \
                        json_extract(summary, '$.message_count') || '|' || \
                        json_extract(summary, '$.line_count') || '|' || \
                        json_extract(summary, '$.input_tokens') || '|' || \
                        json_extract(summary, '$.cached_tokens') || '|' || \
                        json_extract(summary, '$.is_empty') \
                 FROM {from} WHERE id = '{id}'"
            ),
        )
        .await;
        assert_eq!(cols, ["Fix the parser|claude-x|2|2|4|1|false"]);
        let typed = strings(
            &ctx,
            &format!(
                "SELECT title || '|' || model || '|' || message_count || '|' || \
                        line_count || '|' || input_tokens || '|' || cached_tokens || '|' || \
                        is_empty || '|' || output_tokens || '|' || \
                        coalesce(CAST(cost_usd AS VARCHAR), 'null') \
                 FROM {from} WHERE id = '{id}'"
            ),
        )
        .await;
        assert_eq!(
            typed,
            ["Fix the parser|claude-x|2|2|4|1|false|2|null"],
            "the typed columns carry the same values as the summary JSON"
        );
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT json_extract(summary, '$.cache_read_input_tokens') \
                     FROM {from} WHERE id = '{id}'"
                )
            )
            .await,
            ["1"],
            "the driver's extra members ride in the summary"
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT id FROM {from} WHERE is_empty IS TRUE")
            )
            .await,
            [empty]
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT coalesce(title, '-') FROM {from} ORDER BY id")
            )
            .await,
            ["Fix the parser", "-"]
        );
    }

    /// Every `native_session` row names its session type, the value a
    /// stored session carries after `add`, whether or not its
    /// transcript holds anything.
    #[tokio::test]
    async fn native_session_names_its_session_type() {
        let claude_root = tempfile::tempdir().unwrap();
        let dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("dddddddd-0000-0000-0000-000000000001.jsonl"),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"Fix the parser"}}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(dir.join("dddddddd-0000-0000-0000-000000000002.jsonl"), "").unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT session_type FROM native_session('{spec}') ORDER BY id")
            )
            .await,
            ["claude", "claude"]
        );
    }

    /// A file the walk lists is a session whether or not it reads. One
    /// that does not decode is a row whose `summary` carries the error
    /// and whose derived columns are null, so `--empty` never selects
    /// it and a count of the source still includes it.
    #[tokio::test]
    async fn native_session_lists_an_unreadable_transcript_with_its_read_error() {
        let claude_root = tempfile::tempdir().unwrap();
        let dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let good = "eeeeeeee-0000-0000-0000-000000000001";
        std::fs::write(
            dir.join(format!("{good}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let bad = "eeeeeeee-0000-0000-0000-000000000002";
        std::fs::write(
            dir.join(format!("{bad}.jsonl")),
            "this is not a transcript\n",
        )
        .unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let from = format!("native_session('{spec}')");

        assert_eq!(
            strings(&ctx, &format!("SELECT COUNT(*) FROM {from}")).await,
            ["2"]
        );
        let row = strings(
            &ctx,
            &format!(
                "SELECT project || '|' || coalesce(title, 'null') || '|' || \
                        coalesce(CAST(is_empty AS VARCHAR), 'null') || '|' || \
                        coalesce(CAST(message_count AS VARCHAR), 'null') || '|' || \
                        json_extract(summary, '$.read_error') \
                 FROM {from} WHERE id = '{bad}'"
            ),
        )
        .await;
        assert_eq!(row.len(), 1);
        assert!(
            row[0].starts_with("-w-proj|null|null|null|") && row[0].contains("does not decode"),
            "{row:?}"
        );
        assert!(
            strings(
                &ctx,
                &format!("SELECT id FROM {from} WHERE is_empty IS TRUE")
            )
            .await
            .is_empty(),
            "an unread session is not an empty one"
        );
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT id FROM {from} WHERE json_extract(summary, '$.read_error') IS NULL"
                )
            )
            .await,
            [good]
        );
    }

    /// The session listing's SQL shape, columns only under `ORDER BY
    /// mtime DESC LIMIT n`, hands the limit to the walk: the walk cuts
    /// the source to n sessions by mtime before any file is parsed,
    /// which the summary cache shows as n files written.
    #[tokio::test]
    async fn listing_limit_reaches_the_walk_before_any_session_is_parsed() {
        use std::time::{Duration, UNIX_EPOCH};

        let claude_root = tempfile::tempdir().unwrap();
        let dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let line = r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#;
        let ids: Vec<String> = (1..=5)
            .map(|n| format!("ffffffff-0000-0000-0000-00000000000{n}"))
            .collect();
        for (n, id) in ids.iter().enumerate() {
            let path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&path, format!("{line}\n")).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n as u64 * 100))
                .unwrap();
        }
        let cache_root = claude_root.path().join("cache");
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(cache_root.clone())
            .build()
            .await;
        let from = format!("native_session('{spec}')");

        let listing = format!(
            "SELECT id, id_display, id_prefix, project, title, session_type, model, size, \
                    message_count, mtime AS time, '' AS driver \
             FROM {from} ORDER BY mtime DESC LIMIT 2"
        );
        assert_eq!(
            strings(&ctx, &listing).await,
            [ids[4].clone(), ids[3].clone()]
        );
        let parsed = summary_cache_files(&cache_root);
        assert_eq!(parsed, 2, "the walk opened only the limited sessions");

        let plan = explain(&ctx, &listing).await;
        assert!(
            !plan.physical.contains("RepartitionExec") && !plan.physical.contains("SortExec"),
            "a columns-only listing is neither repartitioned nor re-sorted: {}",
            plan.physical
        );
    }

    /// The number of session summaries the driver's cache under
    /// `cache_root` holds: one file per parsed session, beside the
    /// manifest and lock files
    fn summary_cache_files(cache_root: &Path) -> usize {
        fn walk(dir: &Path, out: &mut usize) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), out);
                } else if dir.file_name().is_some_and(|d| d == "summary")
                    && entry.file_name().len() == 36
                {
                    *out += 1;
                }
            }
        }
        let mut n = 0;
        walk(cache_root, &mut n);
        n
    }

    #[tokio::test]
    async fn native_session_pushes_project_and_mtime_filters_into_the_walk() {
        use std::time::{Duration, UNIX_EPOCH};

        let claude_root = tempfile::tempdir().unwrap();
        let line = r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#;
        // (project, id, mtime seconds since the epoch)
        let sessions = [
            (
                "-w-a",
                "aaaaaaaa-0000-0000-0000-000000000001",
                1_700_000_000u64,
            ),
            (
                "-w-a",
                "aaaaaaaa-0000-0000-0000-000000000002",
                1_700_000_100,
            ),
            (
                "-w-b",
                "bbbbbbbb-0000-0000-0000-000000000003",
                1_700_000_200,
            ),
        ];
        for (project, id, secs) in sessions {
            let dir = claude_root.path().join("projects").join(project);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{id}.jsonl"));
            std::fs::write(&path, format!("{line}\n")).unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        }
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let from = format!("native_session('{spec}')");

        let newest_in_a =
            format!("SELECT id FROM {from} WHERE project = '-w-a' ORDER BY mtime DESC LIMIT 1");
        assert_eq!(
            strings(&ctx, &newest_in_a).await,
            ["aaaaaaaa-0000-0000-0000-000000000002"]
        );
        let plan = explain(&ctx, &newest_in_a).await;
        assert!(
            !plan.physical.contains("FilterExec"),
            "the project filter is applied in the walk: {}",
            plan.physical
        );

        let after = format!(
            "SELECT id FROM {from} WHERE mtime > to_timestamp_seconds(1700000050) ORDER BY mtime"
        );
        assert_eq!(
            strings(&ctx, &after).await,
            [
                "aaaaaaaa-0000-0000-0000-000000000002",
                "bbbbbbbb-0000-0000-0000-000000000003"
            ]
        );
    }

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
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
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

    /// `tool_use` reads a stored session through its driver: one row
    /// per call, paired with the result entry, under the scope's
    /// session filter and the `line` filter on the calling entry.
    #[tokio::test]
    async fn tool_use_reads_stored_sessions_through_the_driver() {
        let (_tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-666666666666";
        let project_dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"fix it"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/w/proj/a.rs","old_string":"x","new_string":"y"}}]}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]},"toolUseResult":{"filePath":"/w/proj/a.rs"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/w/proj/b.rs"}}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();
        {
            let store = store.lock().unwrap();
            SessionStore::from(&*store)
                .add(driver.as_ref(), native.as_mut())
                .unwrap();
        }

        let ctx = ContextBuilder::new(Some(store))
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let names = strings(&ctx, "SELECT name FROM tool_use ORDER BY line").await;
        assert_eq!(names, ["Edit", "Read"]);

        let paired = strings(
            &ctx,
            "SELECT m.text FROM tool_use t JOIN message m \
             ON m.session_id = t.session_id AND m.line = t.result_line \
             WHERE t.id = 't1'",
        )
        .await;
        assert_eq!(paired, ["done"]);

        let files = strings(
            &ctx,
            "SELECT tool_use_id || ' ' || type || ' ' || path FROM tool_use_file \
             ORDER BY tool_use_id",
        )
        .await;
        assert_eq!(
            files,
            ["t1 write /w/proj/a.rs", "t2 cite /w/proj/b.rs"],
            "the stored tool_use_file rows come from the same derivation"
        );

        let native_rows = strings(
            &ctx,
            &format!("SELECT name FROM native_tool_use('{spec}') ORDER BY line"),
        )
        .await;
        assert_eq!(native_rows, ["Edit", "Read"]);
    }

    /// `session_change` reads a published session's `changes.json`: one row
    /// per record with the record's fields in the table's columns, a
    /// title record with no reason, under the scope's session filter
    /// and the `line` filter. The original session has no rows.
    #[tokio::test]
    async fn changes_reads_a_published_sessions_records() {
        let (tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-888888888888";
        let project_dir = claude_root.path().join("projects").join("-w-proj");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(format!("{native_id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"hello there"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:02Z","message":{"role":"user","content":"bye"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let (original_id, published_id) = {
            let store = store.lock().unwrap();
            let outcome = SessionStore::from(&*store)
                .add(driver.as_ref(), native.as_mut())
                .unwrap();
            let original = SessionStore::from(&*store)
                .at_commit(&outcome.commit_sha)
                .unwrap();
            // The derived session drops line 2 and redacts the
            // greeting on line 1
            let mut text = String::new();
            SessionStore::from(&*store)
                .content(&outcome.commit_sha)
                .open("session.jsonl")
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            let lines: Vec<String> = text
                .lines()
                .map(|l| l.replace("hello there", "hello [REDACTED]"))
                .collect();
            let derived_id = new_uuid();
            let dir = write_session_dir(
                tmp.path(),
                &derived_id,
                &SessionAttrsRecord {
                    origin: format!("session:{}@{}", original.id, original.commit_sha),
                    ..original.attrs
                },
                concat!(
                    "[\n",
                    r#"  ["line", 2, "scope", "user:t"],"#,
                    "\n",
                    r#"  ["text", 1, "/message/content", 6, 16, "pii", "filter:pii"],"#,
                    "\n",
                    r#"  ["title", 3, "user:t"]"#,
                    "\n]\n",
                ),
                &[("session.jsonl", &format!("{}\n{}\n", lines[0], lines[2]))],
            );
            SessionStore::from(&*store).create_from_dir(&dir).unwrap();
            (outcome.id, derived_id)
        };

        let ctx = ContextBuilder::new(Some(store))
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let rows = strings(
            &ctx,
            "SELECT type || ' ' || CAST(line AS VARCHAR) \
                 || ' ' || COALESCE(CAST(line_end AS VARCHAR), '-') \
                 || ' ' || COALESCE(ref, '-') \
                 || ' ' || COALESCE(CAST(span_start AS VARCHAR), '-') \
                 || ' ' || COALESCE(CAST(span_end AS VARCHAR), '-') \
                 || ' ' || COALESCE(reason, '-') || ' ' || author \
             FROM session_change ORDER BY line, type",
        )
        .await;
        assert_eq!(
            rows,
            [
                "text 1 - /message/content 6 16 pii filter:pii",
                "line 2 2 - - - scope user:t",
                "title 3 - - - - - user:t",
            ]
        );

        let by_session = strings(
            &ctx,
            &format!(
                "SELECT type FROM session_change WHERE session_id = '{published_id}' AND line = 2"
            ),
        )
        .await;
        assert_eq!(by_session, ["line"]);
        let none = strings(
            &ctx,
            &format!("SELECT type FROM session_change WHERE session_id = '{original_id}'"),
        )
        .await;
        assert!(none.is_empty(), "{none:?}");
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
        assert_eq!(note_cols.last().map(String::as_str), Some("work_id"));
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
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
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
                    work_id: None,
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
                    evidence: slice::from_ref(&note_id),
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
            slice::from_ref(&dataset_commit)
        );
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT session_id FROM dataset_session WHERE dataset_id = '{dataset_id}'"
                )
            )
            .await,
            slice::from_ref(&session_id)
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
            strings(&ctx, "SELECT status || ' ' || status_reason FROM issue").await,
            ["closed wontfix"]
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
            slice::from_ref(&note_id)
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT session_id FROM session_issue WHERE issue_id = '{issue_id}'")
            )
            .await,
            slice::from_ref(&session_id)
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
            slice::from_ref(&session_id)
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

    /// `share` lists each share with its description and filter spec,
    /// `share_session_link` carries the share and member commits,
    /// `share_session` joins on ids at the share's current commit, and
    /// a user-facing context hides the link table and the commit.
    #[tokio::test]
    async fn share_tables_list_members_and_hide_commits() {
        let (tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-666666666666";
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
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let (share_id, share_commit, published_id) = {
            let store = store.lock().unwrap();
            let sessions = SessionStore::from(&*store);
            let outcome = sessions.add(driver.as_ref(), native.as_mut()).unwrap();
            let original = sessions.at_commit(&outcome.commit_sha).unwrap();
            let published_id = new_uuid();
            let dir = write_session_dir(
                tmp.path(),
                &published_id,
                &SessionAttrsRecord {
                    origin: format!("session:{}@{}", original.id, original.commit_sha),
                    summary_version: None,
                    publish: Some(PublishAttrs {
                        filters: vec![PublishFilter {
                            name: "secret".into(),
                            version: Some("1.3.0".into()),
                        }],
                    }),
                    ..original.attrs
                },
                "[]\n",
                &[("session.jsonl", "{}\n")],
            );
            sessions.create_from_dir(&dir).unwrap();
            let shares = ShareStore::from(&*store);
            let share_id = shares
                .create(
                    &ShareAttrs {
                        description: Some("Work.".into()),
                        filters: vec!["secret >=1.2".into()],
                    },
                    slice::from_ref(&published_id),
                )
                .unwrap();
            let share_commit = shares.get(&share_id).unwrap().commit_sha;
            (share_id, share_commit, published_id)
        };

        let ctx = ContextBuilder::new(Arc::clone(&store).into()).build().await;
        assert_eq!(
            strings(&ctx, "SELECT description || ' | ' || filters FROM share").await,
            ["Work. | secret >=1.2"]
        );
        assert_eq!(
            strings(&ctx, "SELECT share_commit FROM share_session_link").await,
            slice::from_ref(&share_commit)
        );
        assert_eq!(
            strings(
                &ctx,
                &format!(
                    "SELECT session_id FROM share_session WHERE share_id = '{share_id}' AND session_num = 1"
                )
            )
            .await,
            slice::from_ref(&published_id)
        );
        assert_eq!(
            strings(&ctx, "SELECT commit FROM share").await,
            [share_commit]
        );

        let user = ContextBuilder::new(Some(store))
            .skip_system_cols()
            .build()
            .await;
        assert_eq!(
            strings(
                &user,
                &format!("SELECT session_id FROM share_session WHERE share_id = '{share_id}'")
            )
            .await,
            [published_id]
        );
        assert!(user.sql("SELECT commit FROM share").await.is_err());
        assert!(
            user.sql("SELECT share_id FROM share_session_link")
                .await
                .is_err()
        );
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

    /// A scan directory `scans/<id>/` whose `scan/` tree holds three
    /// tasks and one agent record, with a plan ordering the tasks
    /// differently from tree order. The root `attrs.json` is absent,
    /// as for an active scan.
    fn write_scan_dir(home: &std::path::Path, id: &str) -> std::path::PathBuf {
        let root = home.join("scans").join(id);
        let scan = root.join("scan");
        let write = |path: &str, content: &str| {
            let path = scan.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write(
            "plan.json",
            r#"{"tasks":[
                {"task":"hello:greet","selected":"explicit","after":[],"unmatched_note_wants":[]},
                {"task":"hello:fail","selected":"group:default","after":[],"unmatched_note_wants":[]},
                {"task":"hello:after","selected":"group:eval","after":[],"unmatched_note_wants":[]}
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
        write("tasks/hello/after/attrs.json", r#"{"status":"canceled"}"#);
        write(
            "tasks/hello/greet/agents/AGENT1/attrs.json",
            r#"{"exit_code":0,"is_error":false,"stop_reason":"end_turn","turns":2,"duration_ms":1500,"cost_usd":0.5}"#,
        );
        write("tasks/hello/greet/agents/AGENT1/stderr", "");
        write(
            "tasks/hello/greet/agents/AGENT1/result",
            r#"{"is_error":false,"total_cost_usd":0.5}"#,
        );
        // An agent still running: its record is the native id alone
        write("tasks/hello/greet/agents/AGENT2/native_id", "native-2\n");
        write("logs/out", "hello, world\n");
        write("logs/err", "task hello:fail failed\nboom\n");
        root
    }

    /// `scan_task` lists an active scan's tasks in plan order with
    /// their plan selection, and `scan_task_agent` its agent records
    /// with the result verbatim; neither has a commit. A running
    /// agent is a row with its native id and null result columns.
    #[tokio::test]
    async fn scan_dir_scope_serves_tasks_and_agents() {
        let (tmp, store) = open_store();
        let id = new_uuid();
        let root = write_scan_dir(tmp.path(), &id);
        let ctx = ContextBuilder::new(Some(store))
            .scope(ScanScope::scan_dir(&root))
            .build()
            .await;
        assert!(
            ctx.table_function("native_blame").is_err() && ctx.table_function("blame").is_err(),
            "a scan-scoped context has no blame functions"
        );
        assert_eq!(
            strings(&ctx, "SELECT task FROM scan_task ORDER BY num").await,
            ["greet", "fail", "after"]
        );
        assert_eq!(
            strings(&ctx, "SELECT status FROM scan_task ORDER BY num").await,
            ["completed", "failed", "canceled"]
        );
        assert_eq!(
            strings(&ctx, "SELECT selected FROM scan_task ORDER BY num").await,
            ["explicit", "group:default", "group:eval"]
        );
        assert_eq!(
            strings(
                &ctx,
                "SELECT scan_id FROM scan_task WHERE scan_commit IS NULL"
            )
            .await,
            vec![id.clone(); 3]
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
        assert_eq!(
            strings(
                &ctx,
                "SELECT stop_reason FROM scan_task_agent \
                 WHERE NOT is_error AND turns = 2 AND cost_usd = 0.5"
            )
            .await,
            ["end_turn"]
        );
        assert_eq!(
            strings(
                &ctx,
                "SELECT session_id FROM scan_task_agent \
                 WHERE native_id = 'native-2' AND exit_code IS NULL AND is_error IS NULL"
            )
            .await,
            ["AGENT2"]
        );
    }

    /// `scan_log` serves the logs a scan wrote and no row for one it
    /// did not: from the directory while the scan is active, and from
    /// the object in the store scope and the stored scan scope.
    #[tokio::test]
    async fn scan_log_serves_the_logs_a_scan_wrote_in_every_scope() {
        let (tmp, store) = open_store();
        let id = new_uuid();
        let root = write_scan_dir(tmp.path(), &id);
        let active = ContextBuilder::new(Some(Arc::clone(&store)))
            .scope(ScanScope::scan_dir(&root))
            .build()
            .await;
        assert_eq!(
            strings(&active, "SELECT name FROM scan_log ORDER BY name").await,
            ["err", "out"]
        );
        assert_eq!(
            strings(&active, "SELECT text FROM scan_log WHERE name = 'err'").await,
            ["task hello:fail failed\nboom\n"]
        );

        std::fs::write(
            root.join("scan").join("attrs.json"),
            r#"{"runtime":"gage 1","started":1000,"stopped":1500,"canceled":false,"tasks":{"total":3,"completed":1,"failed":1}}"#,
        )
        .unwrap();
        {
            let store = store.lock().unwrap();
            ScanStore::from(&*store).create_from_dir(&root).unwrap();
        }
        let ctx = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
        assert_eq!(
            strings(
                &ctx,
                "SELECT scan_id || ' ' || name FROM scan_log ORDER BY name"
            )
            .await,
            [format!("{id} err"), format!("{id} out")]
        );
        assert_eq!(
            strings(&ctx, "SELECT text FROM scan_log WHERE name = 'out'").await,
            ["hello, world\n"]
        );
        let scoped = ContextBuilder::new(Some(store))
            .scope(ScanScope::stored(&id))
            .build()
            .await;
        assert_eq!(
            strings(&scoped, "SELECT name FROM scan_log ORDER BY name").await,
            ["err", "out"]
        );
    }

    /// Once the scan is stored, the store scope lists its tasks and
    /// agents under its commit, and the stored scan scope lists the
    /// same rows.
    #[tokio::test]
    async fn stored_scans_serve_tasks_and_agents_in_both_scopes() {
        let (tmp, store) = open_store();
        let id = new_uuid();
        let root = write_scan_dir(tmp.path(), &id);
        std::fs::write(
            root.join("scan").join("attrs.json"),
            r#"{"runtime":"gage 1","started":1000,"stopped":1500,"canceled":false,"tasks":{"total":3,"completed":1,"failed":1}}"#,
        )
        .unwrap();
        let commit = {
            let store = store.lock().unwrap();
            ScanStore::from(&*store).create_from_dir(&root).unwrap()
        };

        let ctx = ContextBuilder::new(Some(Arc::clone(&store))).build().await;
        assert_eq!(
            strings(&ctx, "SELECT task FROM scan_task ORDER BY num").await,
            ["greet", "fail", "after"]
        );
        assert_eq!(
            strings(&ctx, "SELECT DISTINCT scan_commit FROM scan_task").await,
            slice::from_ref(&commit)
        );
        // The running agent's record was never stored
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
            .scope(ScanScope::stored(&id))
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

    /// A stored scan links the commit of each issue it wrote, but the
    /// scope lists the issue's current version: a status change after
    /// the scan shows in `issue` and `issue_event`.
    #[tokio::test]
    async fn stored_scan_scope_serves_issues_at_their_tip() {
        let (tmp, store) = open_store();
        let id = new_uuid();
        let root = write_scan_dir(tmp.path(), &id);
        std::fs::write(
            root.join("scan").join("attrs.json"),
            r#"{"runtime":"gage 1","started":1000,"stopped":1500,"canceled":false,"tasks":{"total":3,"completed":1,"failed":1}}"#,
        )
        .unwrap();
        let tip = {
            let store = store.lock().unwrap();
            let issues = IssueStore::from(&*store);
            let issue_id = issues
                .create(IssueInput {
                    name: "n",
                    title: "t",
                    description: None,
                    author: "task:s:t",
                    status: IssueStatus::Pending,
                    evidence: &[],
                    key: None,
                })
                .unwrap();
            let written = issues.get(&issue_id).unwrap().commit_sha;
            std::fs::write(
                root.join("scan").join("issues.link"),
                format!("{written}\n"),
            )
            .unwrap();
            ScanStore::from(&*store).create_from_dir(&root).unwrap();
            issues
                .set_status(
                    &issue_id,
                    IssueStatus::Closed,
                    Some(StatusReason::Completed),
                    "user:t",
                    None,
                )
                .unwrap();
            issues.get(&issue_id).unwrap().commit_sha
        };

        let scoped = ContextBuilder::new(Some(store))
            .scope(ScanScope::stored(&id))
            .build()
            .await;
        assert_eq!(
            strings(&scoped, "SELECT status || ' ' || status_reason FROM issue").await,
            ["closed completed"]
        );
        assert_eq!(strings(&scoped, "SELECT commit FROM issue").await, [tip]);
        assert_eq!(
            strings(&scoped, "SELECT event FROM issue_event ORDER BY timestamp").await,
            ["create", "status"]
        );
    }

    fn seed_text_session(claude_root: &std::path::Path, id: &str, texts: &[&str]) {
        let dir = claude_root.join("projects").join("-w-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let lines: Vec<String> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                format!(
                    r#"{{"type":"user","uuid":"u{i}","timestamp":"2025-01-01T00:00:0{i}Z","cwd":"/w/proj","message":{{"role":"user","content":"{text}"}}}}"#
                )
            })
            .collect();
        std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    }

    /// `native_message_text` searches a source's index: one row per
    /// matching message, best match first, named by session URL, with
    /// `LIMIT` pushed into the search.
    #[tokio::test]
    async fn native_message_text_searches_a_source() {
        let claude_root = tempfile::tempdir().unwrap();
        let a = "aaaaaaaa-0000-0000-0000-000000000001";
        let b = "bbbbbbbb-0000-0000-0000-000000000002";
        seed_text_session(
            claude_root.path(),
            a,
            &["the quokka smiled", "nothing here"],
        );
        seed_text_session(claude_root.path(), b, &["a quokka and a wombat"]);
        let spec = format!("claude:{}", claude_root.path().display());
        let ctx = ContextBuilder::new(None)
            .cache_root(claude_root.path().join("cache"))
            .build()
            .await;
        let from = format!("native_message_text('{spec}', 'quokka')");

        let urls = strings(&ctx, &format!("SELECT url FROM {from} ORDER BY session_id")).await;
        assert_eq!(
            urls,
            [format!("{spec}/{a}"), format!("{spec}/{b}")],
            "a session URL is the source URL and the native id"
        );
        let snippets = strings(&ctx, &format!("SELECT snippet FROM {from}")).await;
        assert!(
            snippets.iter().all(|s| s.contains("«quokka»")),
            "{snippets:?}"
        );
        assert_eq!(
            strings(&ctx, &format!("SELECT session_id FROM {from} LIMIT 1"))
                .await
                .len(),
            1
        );
        assert_eq!(
            strings(
                &ctx,
                &format!("SELECT CAST(line AS VARCHAR) FROM {from} WHERE session_id = '{a}'")
            )
            .await,
            ["1"]
        );
        assert!(
            strings(
                &ctx,
                &format!(
                    "SELECT CAST(line AS VARCHAR) FROM native_message_text('{spec}', 'wallaby')"
                )
            )
            .await
            .is_empty()
        );
        let err = ctx
            .sql(&format!(
                "SELECT line FROM native_message_text('{spec}', '\"open')"
            ))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid text search query"),
            "{err}"
        );
        let corpus = gage_core::cache::driver_corpus_dir(
            &claude_root.path().join("cache"),
            "claude",
            claude_root.path(),
        );
        assert!(
            corpus.join("text").is_dir(),
            "the index is kept in the source's corpus directory under the context's cache root"
        );
    }

    /// `message_text` searches the store's index over the live tips,
    /// names sessions by `gage:` URL and Gage id, and joins to
    /// `message` on `(session_id, line)`.
    #[tokio::test]
    async fn message_text_searches_the_store() {
        let (tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let native_id = "11111111-2222-3333-4444-555555555555";
        seed_text_session(
            claude_root.path(),
            native_id,
            &["hello there", "the quokka smiled"],
        );
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let mut native = source.open_native(native_id).unwrap();
        let (session_id, store_path) = {
            let store = store.lock().unwrap();
            let id = SessionStore::from(&*store)
                .add(driver.as_ref(), native.as_mut())
                .unwrap()
                .id;
            (id, store.path().to_path_buf())
        };
        let ctx = ContextBuilder::new(Some(Arc::clone(&store)))
            .cache_root(tmp.path().join("cache"))
            .build()
            .await;

        let rows = strings(
            &ctx,
            "SELECT url || ' ' || session_id || ' ' || CAST(line AS VARCHAR) \
             FROM message_text('quokka')",
        )
        .await;
        assert_eq!(
            rows,
            [format!(
                "gage:{}/{session_id} {session_id} 2",
                store_path.display()
            )]
        );
        let joined = strings(
            &ctx,
            "SELECT m.text FROM message_text('quokka') t \
             JOIN message m ON m.session_id = t.session_id AND m.line = t.line",
        )
        .await;
        assert_eq!(joined, ["the quokka smiled"]);
        assert!(
            strings(
                &ctx,
                "SELECT CAST(line AS VARCHAR) FROM message_text('wallaby')"
            )
            .await
            .is_empty()
        );
        let corpus = gage_core::cache::store_corpus_dir(
            &tmp.path().join("cache"),
            &tmp.path().join("store.git"),
        );
        assert!(
            corpus.join("text").is_dir(),
            "the store's index is kept in its corpus directory under the context's cache root"
        );
    }

    /// `gage session find` looks up the timestamps of its shown hits
    /// with literal `session_id` and `line` lists against `message`
    /// and `native_message`. Both tables apply the lists in the scan,
    /// so the plan has no filter step over the parsed rows.
    #[tokio::test]
    async fn message_timestamps_resolve_by_literal_keys() {
        let (tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let a = "aaaaaaaa-0000-0000-0000-000000000001";
        let b = "bbbbbbbb-0000-0000-0000-000000000002";
        seed_text_session(claude_root.path(), a, &["hello there", "the quokka smiled"]);
        seed_text_session(claude_root.path(), b, &["a quokka and a wombat"]);
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let mut native = source.open_native(a).unwrap();
        let stored_id = {
            let store = store.lock().unwrap();
            SessionStore::from(&*store)
                .add(driver.as_ref(), native.as_mut())
                .unwrap()
                .id
        };
        let ctx = ContextBuilder::new(Some(Arc::clone(&store)))
            .cache_root(tmp.path().join("cache"))
            .build()
            .await;

        let native_sql = format!(
            "SELECT session_id || ' ' || CAST(line AS VARCHAR) || ' ' || CAST(timestamp AS VARCHAR) \
             FROM native_message('{spec}') \
             WHERE session_id IN ('{a}') AND line IN (2)"
        );
        assert_eq!(
            strings(&ctx, &native_sql).await,
            [format!("{a} 2 2025-01-01T00:00:01Z")]
        );
        let stored_sql = format!(
            "SELECT session_id || ' ' || CAST(line AS VARCHAR) || ' ' || CAST(timestamp AS VARCHAR) \
             FROM message \
             WHERE session_id IN ('{stored_id}') AND line IN (2)"
        );
        assert_eq!(
            strings(&ctx, &stored_sql).await,
            [format!("{stored_id} 2 2025-01-01T00:00:01Z")]
        );
        for sql in [&native_sql, &stored_sql] {
            let plan = explain(&ctx, sql).await.physical;
            assert!(
                !plan.contains("FilterExec"),
                "the key lists are applied by the scan: {plan}"
            );
        }
    }

    /// A literal `session_id` set on `message_text` or
    /// `native_message_text` restricts the search itself, so a `LIMIT`
    /// counts hits in those sessions alone: a session whose best match
    /// ranks below every hit of a session outside the set is still
    /// found. The set is applied in the scan, so the plan has no
    /// filter step and the limit reaches the search.
    #[tokio::test]
    async fn message_text_restricts_the_search_to_literal_sessions() {
        let (tmp, store) = open_store();
        let claude_root = tempfile::tempdir().unwrap();
        let strong = "aaaaaaaa-0000-0000-0000-000000000001";
        let weak = "bbbbbbbb-0000-0000-0000-000000000002";
        seed_text_session(
            claude_root.path(),
            strong,
            &["quokka quokka quokka", "quokka quokka", "quokka"],
        );
        seed_text_session(claude_root.path(), weak, &["a quokka among other words"]);
        let spec = format!("claude:{}", claude_root.path().display());
        let registry = DriverRegistry::builtin();
        let driver = registry.driver_for(&spec).unwrap();
        let source = driver
            .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
            .unwrap();
        let stored_weak = {
            let store = store.lock().unwrap();
            let sessions = SessionStore::from(&*store);
            let mut native = source.open_native(strong).unwrap();
            sessions.add(driver.as_ref(), native.as_mut()).unwrap();
            let mut native = source.open_native(weak).unwrap();
            sessions.add(driver.as_ref(), native.as_mut()).unwrap().id
        };
        let ctx = ContextBuilder::new(Some(Arc::clone(&store)))
            .cache_root(tmp.path().join("cache"))
            .build()
            .await;

        let unrestricted =
            format!("SELECT session_id FROM native_message_text('{spec}', 'quokka') LIMIT 1");
        assert_eq!(strings(&ctx, &unrestricted).await, [strong]);
        let native_sql = format!(
            "SELECT session_id FROM native_message_text('{spec}', 'quokka') \
             WHERE session_id IN ('{weak}', 'cccccccc-0000-0000-0000-000000000003') LIMIT 1"
        );
        assert_eq!(strings(&ctx, &native_sql).await, [weak]);
        let stored_sql = format!(
            "SELECT session_id FROM message_text('quokka') \
             WHERE session_id = '{stored_weak}' LIMIT 1"
        );
        assert_eq!(strings(&ctx, &stored_sql).await, [stored_weak.as_str()]);
        for sql in [&native_sql, &stored_sql] {
            let plan = explain(&ctx, sql).await.physical;
            assert!(
                !plan.contains("FilterExec"),
                "the session set is applied by the search: {plan}"
            );
        }
        // The type and subtype columns are raw terms too, so their
        // literal sets reach the search alongside the session set
        let typed_sql = format!(
            "SELECT session_id FROM native_message_text('{spec}', 'quokka') \
             WHERE session_id = '{weak}' AND type = 'user' AND subtype = 'text' LIMIT 1"
        );
        assert_eq!(strings(&ctx, &typed_sql).await, [weak]);
        let plan = explain(&ctx, &typed_sql).await.physical;
        assert!(!plan.contains("FilterExec"), "{plan}");
        assert!(
            strings(
                &ctx,
                &format!(
                    "SELECT session_id FROM native_message_text('{spec}', 'quokka') \
                     WHERE type = 'assistant'"
                )
            )
            .await
            .is_empty()
        );
        // A filter the search cannot apply stays a filter step
        let plan = explain(
            &ctx,
            &format!(
                "SELECT session_id FROM native_message_text('{spec}', 'quokka') \
                 WHERE session_id LIKE 'b%'"
            ),
        )
        .await
        .physical;
        assert!(plan.contains("FilterExec"), "{plan}");
    }
}
