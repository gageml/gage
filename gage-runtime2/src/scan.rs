//! `scan()` and `params()`: the active scan, its sessions, and the
//! scanner's params, for scanners.
//!
//! `scan()` returns the [`Scan`]: its id and its dataset.
//! `scan().sessions()` is a [`SessionsQuery`]; awaiting it reads the
//! dataset's members at the commit the scan links and yields a
//! [`Sessions`] iterator of [`Session`] values. The order is the
//! store's read order, which scanners must not rely on. Every task
//! runs under a [`ScanContext`], scoped by the orchestrator through
//! [`SCAN_CTX`]; `scan()` outside one is a VM error.
//!
//! `params()` returns the scanner's resolved params as an object: the
//! declared defaults with the scan's per-scanner overrides applied, or
//! an empty object for a scanner that declares none.

use std::path::Path;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::error::DataFusionError;
use datafusion::prelude::SessionContext;
use gage_mcp2::{HostError, McpHost};
use gage_query2::{ContextBuilder, ScanScope};
use gage_runtime::datetime::{self, DateTime};
use gage_runtime::error::Error;
use gage_runtime::value::json_to_value;
use gage_session::Driver;
use gage_store::{ScanDirLayout, Store, StoreError};
use rune::Sources;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Object, Protocol, Value, VmError};
use rune::{Any, ContextError, Module};
use serde_json as json;
use tokio::sync::OnceCell;

use crate::tool::QueryScope;

tokio::task_local! {
    /// The running task's scan, read by [`scan`]
    pub static SCAN_CTX: ScanContext;
}

/// The scan a task runs under: its id, its dataset, and the store
/// the dataset is read from. The store's git reader is
/// single-threaded, so each handle is shared under a lock. The
/// runtime's own reads and scan directory writes go through `store`, an
/// async lock, so a task waiting for it parks instead of holding a
/// runtime thread. The query context over the dataset's members has
/// its own handle, since its table providers read synchronously; it
/// is built on first use and shared by every task, so a session's
/// rows are derived once per scan.
#[derive(Clone)]
pub struct ScanContext {
    pub scan_id: String,
    /// `None` when the scan has no dataset; `sessions()` is then empty
    pub dataset: Option<ScanDatasetRef>,
    /// The resolved params of the scanner whose task runs under this
    /// context; `None` when the scanner declares none. The
    /// orchestrator sets it per task, since params are per scanner
    /// and the context is otherwise per scan.
    pub params: Option<json::Value>,
    /// The scanner and task running under this context, set by the
    /// orchestrator per task. Agent sessions are attributed to them.
    pub scanner: String,
    pub task: String,
    /// The Rune sources of the scanner's compiled unit, set by the
    /// orchestrator per task. Used to render `VmError` with source
    /// excerpts, resolved field names, and a backtrace.
    pub sources: Option<Arc<Sources>>,
    /// The driver that runs the scan's agents
    pub driver: Arc<dyn Driver>,
    pub store: Arc<tokio::sync::Mutex<Store>>,
    /// The scan directory: where the runtime writes during the run
    /// and the source of the scan-scoped query context
    pub paths: ScanDirLayout,
    /// Ignore prior work: `hwm` reports 0 for every object and
    /// `carry_forward_notes` links nothing. Watermarks are still
    /// written, so the next scan resumes from this one.
    pub invalidate: bool,
    query_store: Arc<Mutex<Store>>,
    /// The scan-scoped query context, built on the first read
    scan_query: Arc<OnceCell<SessionContext>>,
    /// The store-scoped query context, built on the first read that
    /// consults prior scans
    store_query: Arc<OnceCell<SessionContext>>,
    /// The MCP host serving scanner-defined tools to agents, started
    /// by the first `call_agent` that declares tools
    mcp_host: Arc<OnceCell<McpHost>>,
}

impl ScanContext {
    /// Open the context over the store at `store_path`: one handle for
    /// the runtime and one for the query context.
    pub fn new(
        scan_id: String,
        dataset: Option<ScanDatasetRef>,
        store_path: &Path,
        paths: ScanDirLayout,
        driver: Arc<dyn Driver>,
    ) -> Result<Self, StoreError> {
        Ok(ScanContext {
            scan_id,
            dataset,
            params: None,
            scanner: String::new(),
            task: String::new(),
            sources: None,
            driver,
            store: Arc::new(tokio::sync::Mutex::new(Store::open(store_path)?)),
            paths,
            invalidate: false,
            query_store: Arc::new(Mutex::new(Store::open(store_path)?)),
            scan_query: Arc::new(OnceCell::new()),
            store_query: Arc::new(OnceCell::new()),
            mcp_host: Arc::new(OnceCell::new()),
        })
    }

    pub(crate) async fn mcp_host(&self) -> Result<&McpHost, HostError> {
        self.mcp_host
            .get_or_try_init(|| async {
                let host = McpHost::start().await?;
                tracing::info!(addr = %host.addr(), "mcp host started");
                Ok(host)
            })
            .await
    }

    /// The scan-scoped query context: the scan's sessions at the
    /// commits it links, the notes it wrote or carried, the issues it
    /// wrote, and the relations among them, read from the scan
    /// directory and the store on each query. Every scanner-facing
    /// read and the agent's Query tool run here.
    pub(crate) async fn scan_context(&self) -> Result<&SessionContext, VmError> {
        self.scan_query
            .get_or_try_init(|| async {
                let ctx = ContextBuilder::new(Some(Arc::clone(&self.query_store)))
                    .scope(ScanScope::scan_dir(self.paths.root()))
                    .build()
                    .await;
                tracing::info!("scan query context built");
                Ok(ctx)
            })
            .await
    }

    /// The store-scoped query context, for the reads that consult
    /// prior scans: `hwm` and `carry_forward_notes`.
    pub(crate) async fn store_context(&self) -> Result<&SessionContext, VmError> {
        self.store_query
            .get_or_try_init(|| async {
                let ctx = ContextBuilder::new(Some(Arc::clone(&self.query_store)))
                    .build()
                    .await;
                tracing::info!("store query context built");
                Ok(ctx)
            })
            .await
    }

    /// The context the Gage query tool runs over for `scope`: the
    /// scan's own context, or a scan-scoped context narrowed to one
    /// member, with `entry` and `message` narrowed to the line range
    /// when one is given.
    pub(crate) async fn query_tool_context(
        &self,
        scope: &QueryScope,
    ) -> Result<SessionContext, Error> {
        let agent_err = |e: VmError| Error::agent(format!("Query tool: {e}"));
        match scope {
            QueryScope::Dataset => Ok(self.scan_context().await.map_err(agent_err)?.clone()),
            QueryScope::Session { id, lines } => {
                if self.member_commit(id).await.map_err(agent_err)?.is_none() {
                    return Err(Error::agent(format!(
                        "Query tool: session {id} is not a member of the scan's dataset"
                    )));
                }
                let ctx = ContextBuilder::new(Some(Arc::clone(&self.query_store)))
                    .scope(ScanScope::scan_dir(self.paths.root()).session(id))
                    .build()
                    .await;
                if let Some((start, end)) = lines {
                    restrict_lines(&ctx, *start, *end)
                        .await
                        .map_err(|e| Error::agent(format!("Query tool: {e}")))?;
                }
                Ok(ctx)
            }
        }
    }

    /// The commit the scan reads for the member session `session_id`,
    /// or `None` when it is not a member.
    pub(crate) async fn member_commit(&self, session_id: &str) -> Result<Option<String>, VmError> {
        Ok(members(self.scan_context().await?, false)
            .await?
            .into_iter()
            .find(|s| s.id == session_id)
            .map(|s| s.commit))
    }
}

/// The dataset a scan links: the object id and the commit it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDatasetRef {
    pub id: String,
    pub commit_sha: String,
}

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("scan", scan).build()?;
    m.function("params", params).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<Scan>()?;
    m.function_meta(Scan::sessions)?;
    m.function_meta(Scan::debug)?;
    m.ty::<ScanDataset>()?;
    m.function_meta(ScanDataset::debug)?;
    m.ty::<SessionsQuery>()?;
    m.function_meta(SessionsQuery::newest_first)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionsQuery| async move {
        fetch_sessions(q).await
    })?;
    m.function_meta(crate::validate::sessions_hwm)?;
    m.function_meta(crate::validate::sessions_unseen)?;
    m.ty::<Session>()?;
    m.function_meta(Session::attrs)?;
    m.function_meta(Session::native)?;
    m.function_meta(Session::debug)?;
    m.ty::<NativeQuery>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: NativeQuery| async move {
        fetch_native(q).await
    })?;
    m.ty::<Native>()?;
    m.function_meta(Native::debug)?;
    m.ty::<SessionAttrsQuery>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionAttrsQuery| async move {
        fetch_attrs(q).await
    })?;
    m.ty::<SessionAttrs>()?;
    m.function_meta(SessionAttrs::debug)?;
    m.ty::<Sessions>()?;
    m.function_meta(Sessions::next__meta)?;
    m.function_meta(Sessions::nth__meta)?;
    m.function_meta(Sessions::size_hint__meta)?;
    m.function_meta(Sessions::len__meta)?;
    m.function_meta(Sessions::next_back__meta)?;
    m.implement_trait::<Sessions>(rune::item!(::std::iter::Iterator))?;
    m.implement_trait::<Sessions>(rune::item!(::std::iter::DoubleEndedIterator))?;
    m.implement_trait::<Sessions>(rune::item!(::std::iter::ExactSizeIterator))?;
    datetime::register_types(&mut m)?;
    Ok(m)
}

/// The active scan.
fn scan() -> Result<Scan, VmError> {
    let ctx = current()?;
    Ok(Scan {
        id: ctx.scan_id,
        dataset: ctx.dataset.map(|d| ScanDataset { id: d.id }),
    })
}

fn params() -> Result<Value, VmError> {
    let ctx = current()?;
    Ok(match &ctx.params {
        Some(params) => json_to_value(params),
        None => rune::to_value(Object::new()).unwrap(),
    })
}

pub(crate) fn current() -> Result<ScanContext, VmError> {
    SCAN_CTX
        .try_with(|ctx| ctx.clone())
        .map_err(|_outside_scope| {
            VmError::panic("scan() is available only inside a active scan task")
        })
}

/// The query context over the dataset of whichever context the caller
/// runs under: a scan task's scan-scoped context, or an attachment
/// function's dataset-scoped context. The session and attachment
/// reads go through here so both contexts serve them.
pub(crate) async fn dataset_query() -> Result<SessionContext, VmError> {
    if let Ok(ctx) = SCAN_CTX.try_with(|ctx| ctx.clone()) {
        return Ok(ctx.scan_context().await?.clone());
    }
    if let Ok(ctx) = crate::attach::ATTACH_CTX.try_with(|ctx| ctx.clone()) {
        return Ok(ctx.dataset_context().await?.clone());
    }
    Err(VmError::panic(
        "sessions and attachments are available only inside a scan task or an attachment function",
    ))
}

/// The driver of whichever context the caller runs under.
pub(crate) fn driver_handle() -> Result<Arc<dyn Driver>, VmError> {
    if let Ok(driver) = SCAN_CTX.try_with(|ctx| Arc::clone(&ctx.driver)) {
        return Ok(driver);
    }
    if let Ok(driver) = crate::attach::ATTACH_CTX.try_with(|ctx| Arc::clone(&ctx.driver)) {
        return Ok(driver);
    }
    Err(VmError::panic(
        "the driver is available only inside a scan task or an attachment function",
    ))
}

/// The store of whichever context the caller runs under.
pub(crate) fn store_handle() -> Result<Arc<tokio::sync::Mutex<Store>>, VmError> {
    if let Ok(store) = SCAN_CTX.try_with(|ctx| Arc::clone(&ctx.store)) {
        return Ok(store);
    }
    if let Ok(store) = crate::attach::ATTACH_CTX.try_with(|ctx| Arc::clone(&ctx.store)) {
        return Ok(store);
    }
    Err(VmError::panic(
        "the store is available only inside a scan task or an attachment function",
    ))
}

/// A target argument as the Gage URL the store expects: a `Session`
/// is `session:<id>`; a string with a scheme is taken as given; a bare
/// id or unique prefix resolves to the object's type and full id. The
/// inner error is the scanner's: a value of the wrong type, or an id
/// that names nothing.
pub(crate) async fn target_url(value: &Value) -> Result<Result<String, Error>, VmError> {
    if let Ok(s) = value.borrow_ref::<Session>() {
        return Ok(Ok(format!("session:{}", s.id)));
    }
    let Ok(text) = value.borrow_string_ref() else {
        return Ok(Err(Error::Args(format!(
            "target: expected a Session or a string, got {}",
            value.type_info()
        ))));
    };
    let text = text.to_string();
    if text.contains(':') {
        return Ok(Ok(text));
    }
    let store = store_handle()?;
    let store = store.lock().await;
    let found = match store.resolve_in(&text, None) {
        Ok(found) => found,
        Err(e) => return Ok(Err(Error::Args(format!("target {text}: {e}")))),
    };
    if found.deleted {
        return Ok(Err(Error::Args(format!(
            "target {text}: object is deleted: {}",
            found.id
        ))));
    }
    let type_name = found
        .object_type
        .strip_prefix("gage::")
        .unwrap_or(&found.object_type);
    Ok(Ok(format!("{type_name}:{}", found.id)))
}

/// Render a `VmError` as Rune does: the diagnostic with its source
/// excerpt, then a `Backtrace:` section listing every frame. Without
/// sources the renderer falls back to the error's `Display` form.
pub fn render_vm_error(e: &VmError, sources: Option<&Sources>) -> String {
    let Some(sources) = sources else {
        return e.to_string();
    };
    let mut buf = rune::termcolor::Buffer::no_color();
    e.emit(&mut buf, sources).unwrap();
    String::from_utf8(buf.into_inner()).unwrap()
}

/// Re-register the row tables as views over lines `start` through
/// `end`, inclusive. A view's plan holds the provider it was planned
/// over, so replacing the name does not change what the view reads.
pub(crate) async fn restrict_lines(
    ctx: &SessionContext,
    start: u64,
    end: u64,
) -> Result<(), DataFusionError> {
    for table in ["entry", "message"] {
        let view = ctx
            .sql(&format!(
                "SELECT * FROM {table} WHERE line >= {start} AND line <= {end}"
            ))
            .await?
            .into_view();
        ctx.deregister_table(table)?;
        ctx.register_table(table, view)?;
    }
    Ok(())
}

/// The active scan, as a scanner sees it.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Scan {
    #[rune(get)]
    pub id: String,
    /// The dataset the scan links; `None` when it has none
    #[rune(get)]
    pub dataset: Option<ScanDataset>,
}

impl Scan {
    /// The scan's sessions: the members of its dataset, read when
    /// awaited.
    #[rune::function(instance)]
    fn sessions(&self) -> SessionsQuery {
        SessionsQuery {
            newest_first: false,
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "Scan {{ id: {:?}, dataset: ", self.id)?;
        match &self.dataset {
            Some(dataset) => write!(f, "Some(ScanDataset {{ id: {:?} }})", dataset.id)?,
            None => write!(f, "None")?,
        }
        write!(f, " }}")?;
        Ok(())
    }
}

/// The dataset a scan links.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct ScanDataset {
    #[rune(get)]
    pub id: String,
}

impl ScanDataset {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "ScanDataset {{ id: {:?} }}", self.id)?;
        Ok(())
    }
}

impl rune::alloc::prelude::TryClone for ScanDataset {
    fn try_clone(&self) -> Result<Self, rune::alloc::Error> {
        Ok(self.clone())
    }
}

/// The value of `scan().sessions()`. Awaiting it runs the read.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionsQuery {
    #[rune(skip)]
    pub(crate) newest_first: bool,
}

impl SessionsQuery {
    /// Order the sessions newest-modified first instead of member
    /// order.
    #[rune::function(instance)]
    fn newest_first(mut self) -> Self {
        self.newest_first = true;
        self
    }
}

/// The `ORDER BY` clause for a session read: none for member order.
pub(crate) fn session_order(newest_first: bool) -> &'static str {
    if newest_first {
        " ORDER BY modified DESC"
    } else {
        ""
    }
}

/// The scan's sessions, read from the scoped `session` table, whose
/// rows are the members in member order unless `newest_first` is set.
/// Only the id, the version, and the line count are held per session;
/// `attrs()` reads the rest on request.
async fn fetch_sessions(query: SessionsQuery) -> Result<Sessions, VmError> {
    Ok(Sessions::new(
        members(&dataset_query().await?, query.newest_first).await?,
    ))
}

/// The dataset's sessions, in member order unless `newest_first` is
/// set, read from the `session` table of `df_ctx`.
pub(crate) async fn members(
    df_ctx: &SessionContext,
    newest_first: bool,
) -> Result<Vec<Session>, VmError> {
    let sql = format!(
        "SELECT id, locator, line_count FROM session{}",
        session_order(newest_first)
    );
    let batches = run(df_ctx, &sql).await?;
    let mut items = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let locators = string_column(batch, 1);
        let lines = batch
            .column(2)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("line_count is an integer column");
        for i in 0..batch.num_rows() {
            let line_count = lines.is_valid(i).then(|| lines.value(i));
            items.push(Session::from_row(
                ids.value(i),
                locators.value(i),
                line_count,
            ));
        }
    }
    Ok(items)
}

/// `s` as a single-quoted SQL string literal's content.
pub(crate) fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

/// Run `sql` on a query context.
pub(crate) async fn run(
    df_ctx: &SessionContext,
    sql: &str,
) -> Result<Vec<datafusion::arrow::record_batch::RecordBatch>, VmError> {
    let fail = |e: datafusion::error::DataFusionError| VmError::panic(format!("{sql}: {e}"));
    df_ctx
        .sql(sql)
        .await
        .map_err(fail)?
        .collect()
        .await
        .map_err(fail)
}

pub(crate) fn string_column(
    batch: &datafusion::arrow::record_batch::RecordBatch,
    i: usize,
) -> &StringArray {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("session table column types are fixed")
}

/// A stored session, as a scanner sees it: its id, its line count,
/// and, held for the runtime, the version the scan reads. Everything
/// else is read on request through [`Session::attrs`].
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Session {
    /// The Gage object id
    #[rune(get)]
    pub id: String,
    /// The lines the session holds at the commit the scan reads
    #[rune(get)]
    pub line_count: i64,
    #[rune(skip)]
    pub commit: String,
}

impl Session {
    /// A session from its `id`, `locator`, and `line_count` columns.
    /// Every session written by a line-structured driver carries a
    /// line count; its absence is a store fault.
    pub(crate) fn from_row(id: &str, locator: &str, line_count: Option<i64>) -> Session {
        let commit = locator
            .strip_prefix("git:")
            .unwrap_or_else(|| panic!("session locator is git:<sha>, got {locator:?}"));
        let line_count = line_count.unwrap_or_else(|| {
            panic!("session {id} has no line_count; its driver does not report one")
        });
        Session {
            id: id.to_string(),
            line_count,
            commit: commit.to_string(),
        }
    }

    /// The session's attributes, read when awaited.
    #[rune::function(instance)]
    fn attrs(&self) -> SessionAttrsQuery {
        SessionAttrsQuery {
            id: self.id.clone(),
        }
    }

    /// The native session this stored one was read from, when its
    /// source is reachable on this machine: `Some(Native)` or `None`.
    #[rune::function(instance)]
    fn native(&self) -> NativeQuery {
        NativeQuery {
            id: self.id.clone(),
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "Session {{ id: {:?} }}", self.id)?;
        Ok(())
    }
}

/// The session id in a scanner's argument: a [`Session`] or an id
/// string. The value is borrowed, not taken.
pub(crate) fn session_id(v: &Value) -> Result<String, VmError> {
    if let Ok(s) = v.borrow_ref::<Session>() {
        return Ok(s.id.clone());
    }
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(s.to_string());
    }
    Err(VmError::panic(format!(
        "expected a Session or session id string, got {}",
        v.type_info()
    )))
}

/// The value of `session.native()`. Awaiting it reopens the source.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct NativeQuery {
    #[rune(skip)]
    id: String,
}

/// A stored session's native counterpart, as the source on this
/// machine presents it. A stored session has no location; this is
/// where it came from, and it exists only where that source is.
#[derive(Any, Clone, Debug)]
#[rune(item = ::gage)]
pub struct Native {
    /// The Gage URL of the native session, as stored
    #[rune(get)]
    pub source: String,
    /// The directory of the session's project, when the source records
    /// one
    #[rune(get)]
    pub project_dir: Option<String>,
}

impl Native {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Native {{ source: {:?}, project_dir: {:?} }}",
            self.source, self.project_dir
        )?;
        Ok(())
    }
}

/// Reopen the session's native source through the context's driver
/// and ask it for the project directory. A source the driver cannot
/// open here, or a session from another driver, is `None`: the native
/// session is not on this machine.
async fn fetch_native(q: NativeQuery) -> Result<Option<Native>, VmError> {
    let sql = format!(
        "SELECT driver, native_source, project FROM session WHERE id = '{}'",
        sql_str(&q.id)
    );
    let batches = run(&dataset_query().await?, &sql).await?;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        return Err(VmError::panic(format!(
            "session {} is not a member of the dataset",
            q.id
        )));
    };
    let driver_attr = string_column(batch, 0).value(0).to_string();
    let source_url = string_column(batch, 1).value(0).to_string();
    let projects = string_column(batch, 2);
    let project = projects.is_valid(0).then(|| projects.value(0).to_string());

    let driver = driver_handle()?;
    if driver_attr.split(' ').next() != Some(driver.name()) {
        tracing::debug!(session = %q.id, driver = %driver_attr, "native: another driver's session");
        return Ok(None);
    }
    let source = match driver.open_native_source(&source_url) {
        Ok(source) => source,
        Err(e) => {
            tracing::debug!(session = %q.id, source = %source_url, error = %e, "native: source not reachable");
            return Ok(None);
        }
    };
    let project_dir = match project {
        Some(name) => match source.project_path(&name) {
            Ok(path) => path.map(|p| p.to_string_lossy().into_owned()),
            Err(e) => {
                tracing::debug!(session = %q.id, project = %name, error = %e, "native: no project path");
                None
            }
        },
        None => None,
    };
    Ok(Some(Native {
        source: source_url,
        project_dir,
    }))
}

/// The value of `session.attrs()`. Awaiting it reads the session's
/// row of the `session` table.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionAttrsQuery {
    #[rune(skip)]
    id: String,
}

/// A session's attributes: the user-facing row of the `session`
/// table, read for one session.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct SessionAttrs {
    #[rune(get)]
    pub id: String,
    /// The project, as the driver names it
    #[rune(get)]
    pub project: Option<String>,
    /// When the store wrote this version
    #[rune(get)]
    pub modified: DateTime,
    /// When the store first added the session
    #[rune(get)]
    pub created: DateTime,
    /// When the native artifact was last touched at its source
    #[rune(get)]
    pub native_mtime: DateTime,
    #[rune(get)]
    pub native_size: i64,
    /// The id the harness gave the session
    #[rune(get)]
    pub native_id: String,
    /// The Gage URL the session was read from
    #[rune(get)]
    pub native_source: String,
    #[rune(get)]
    pub session_type: String,
    #[rune(get)]
    pub driver: String,
    #[rune(get)]
    pub title: Option<String>,
    #[rune(get)]
    pub model: Option<String>,
    #[rune(get)]
    pub message_count: Option<i64>,
    #[rune(get)]
    pub is_empty: bool,
    /// The number of lines in the native content, when the driver
    /// reports it
    #[rune(get)]
    pub line_count: Option<i64>,
}

impl SessionAttrs {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "SessionAttrs {{ id: {:?}, project: {:?}, modified: {}, created: {}, \
             native_mtime: {}, native_size: {}, native_id: {:?}, native_source: {:?}, \
             session_type: {:?}, driver: {:?}, title: {:?}, model: {:?}, \
             message_count: {:?}, is_empty: {}, line_count: {:?} }}",
            self.id,
            self.project,
            self.modified.to_rfc3339(),
            self.created.to_rfc3339(),
            self.native_mtime.to_rfc3339(),
            self.native_size,
            self.native_id,
            self.native_source,
            self.session_type,
            self.driver,
            self.title,
            self.model,
            self.message_count,
            self.is_empty,
            self.line_count
        )?;
        Ok(())
    }
}

/// One session's row. A session that is not a member of the scan is
/// a VM error: the value came from `sessions()`, so its absence is a
/// runtime fault.
async fn fetch_attrs(q: SessionAttrsQuery) -> Result<SessionAttrs, VmError> {
    let sql = format!(
        "SELECT id, project, modified, created, native_mtime, native_size, native_id, \
                native_source, session_type, driver, title, model, message_count, is_empty, \
                line_count \
         FROM session WHERE id = '{}'",
        sql_str(&q.id)
    );
    let batches = run(&dataset_query().await?, &sql).await?;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        return Err(VmError::panic(format!(
            "session {} is not a member of the dataset",
            q.id
        )));
    };
    let string = |i: usize| string_column(batch, i);
    let optional = |i: usize| {
        let arr = string_column(batch, i);
        arr.is_valid(0).then(|| arr.value(0).to_string())
    };
    let timestamp = |i: usize| {
        DateTime::from_millis(
            batch
                .column(i)
                .as_any()
                .downcast_ref::<TimestampMillisecondArray>()
                .expect("session table timestamp columns are fixed")
                .value(0),
        )
    };
    let int = |i: usize| {
        batch
            .column(i)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("session table integer columns are fixed")
    };
    let counts = int(12);
    let lines = int(14);
    Ok(SessionAttrs {
        id: string(0).value(0).to_string(),
        project: optional(1),
        modified: timestamp(2),
        created: timestamp(3),
        native_mtime: timestamp(4),
        native_size: int(5).value(0),
        native_id: string(6).value(0).to_string(),
        native_source: string(7).value(0).to_string(),
        session_type: string(8).value(0).to_string(),
        driver: string(9).value(0).to_string(),
        title: optional(10),
        model: optional(11),
        message_count: counts.is_valid(0).then(|| counts.value(0)),
        is_empty: batch
            .column(13)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("is_empty is a boolean column")
            .value(0),
        line_count: lines.is_valid(0).then(|| lines.value(0)),
    })
}

/// A double-ended, exact-size iterator over a scan's sessions, in
/// the order the query read them.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Sessions {
    #[rune(skip)]
    items: Vec<Session>,
    #[rune(skip)]
    front: usize,
    #[rune(skip)]
    back: usize,
}

impl Sessions {
    fn new(items: Vec<Session>) -> Self {
        let back = items.len();
        Sessions {
            items,
            front: 0,
            back,
        }
    }

    #[rune::function(instance, keep, protocol = NEXT)]
    fn next(&mut self) -> Option<Session> {
        if self.front == self.back {
            return None;
        }
        let value = self.items.get(self.front)?.clone();
        self.front += 1;
        Some(value)
    }

    #[rune::function(instance, keep, protocol = NTH)]
    fn nth(&mut self, n: usize) -> Option<Session> {
        let n = self.front.checked_add(n)?;
        if n >= self.back {
            self.front = self.back;
            return None;
        }
        let value = self.items.get(n)?.clone();
        self.front = n + 1;
        Some(value)
    }

    #[rune::function(instance, keep, protocol = SIZE_HINT)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back - self.front;
        (len, Some(len))
    }

    #[rune::function(instance, keep, protocol = LEN)]
    fn len(&self) -> usize {
        self.back - self.front
    }

    #[rune::function(instance, keep, protocol = NEXT_BACK)]
    fn next_back(&mut self) -> Option<Session> {
        if self.front == self.back {
            return None;
        }
        self.back -= 1;
        Some(self.items.get(self.back)?.clone())
    }
}

#[cfg(test)]
mod tests {
    use rune::runtime::Vm;
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources};

    use super::*;

    fn vm(script: &str) -> Vm {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        Vm::new(rt, RuneArc::try_new(unit).unwrap())
    }

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            line_count: 1,
            commit: format!("commit-{id}"),
        }
    }

    /// After `restrict_lines`, the row tables answer only for the
    /// range, under their original names.
    #[tokio::test]
    async fn restrict_lines_narrows_the_row_tables_to_the_range() {
        use datafusion::arrow::array::{Int64Array, StringArray};
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::arrow::record_batch::RecordBatch;
        use datafusion::datasource::MemTable;

        let ctx = SessionContext::new();
        for table in ["entry", "message"] {
            let schema = Arc::new(Schema::new(vec![
                Field::new("line", DataType::Int64, false),
                Field::new("text", DataType::Utf8, false),
            ]));
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(Int64Array::from(vec![1, 2, 3, 4])),
                    Arc::new(StringArray::from(vec!["a", "b", "c", "d"])),
                ],
            )
            .unwrap();
            ctx.register_table(
                table,
                Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
            )
            .unwrap();
        }
        restrict_lines(&ctx, 2, 3).await.unwrap();
        for table in ["entry", "message"] {
            let batches = ctx
                .sql(&format!("SELECT line FROM {table} ORDER BY line"))
                .await
                .unwrap()
                .collect()
                .await
                .unwrap();
            let lines: Vec<i64> = batches
                .iter()
                .flat_map(|b| {
                    b.column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .values()
                        .to_vec()
                })
                .collect();
            assert_eq!(lines, [2, 3], "{table}");
        }
    }

    /// The iterator surface reaches Rune by name through the trait
    /// registrations: `len`, `rev`, `collect`, and the getters.
    #[test]
    fn sessions_iterate_in_order_with_the_full_iterator_surface() {
        let mut vm = vm(r#"
            pub fn check(sessions) {
                let n = sessions.len();
                let ids = sessions.rev().map(|s| s.id).collect::<Vec>();
                (n, ids)
            }
            "#);
        let sessions = Sessions::new(vec![session("a"), session("b"), session("c")]);
        let output = vm.call(["check"], (sessions,)).unwrap();
        let (n, ids): (i64, Vec<String>) = rune::from_value(output).unwrap();
        assert_eq!(n, 3);
        assert_eq!(ids, ["c", "b", "a"]);
    }

    #[test]
    fn session_exposes_its_id_and_debug_form() {
        let mut vm = vm("pub fn check(s) { (s.id, format!(\"{s:?}\")) }");
        let output = vm.call(["check"], (session("a"),)).unwrap();
        let (id, text): (String, String) = rune::from_value(output).unwrap();
        assert_eq!(id, "a");
        assert_eq!(text, "Session { id: \"a\" }");
    }

    /// `scan()` outside a task is a VM error, not a panic.
    #[test]
    fn scan_outside_a_task_is_a_vm_error() {
        let mut vm = vm("use gage::scan; pub fn check() { scan().id }");
        let err = vm.call(["check"], ()).unwrap_err();
        assert!(
            err.to_string()
                .contains("scan() is available only inside a active scan task"),
            "{err}"
        );
    }

    /// With sources, `render_vm_error` resolves the missing field name,
    /// shows the source excerpt, and appends a `Backtrace:` section.
    /// Without sources it falls back to the `Display` form, which
    /// carries only the kind-level message.
    #[test]
    fn render_vm_error_with_sources_names_fields_and_shows_backtrace() {
        let script = r#"
            pub fn check(inputs) { inputs.session_id }
            "#;
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        let mut vm = Vm::new(rt, RuneArc::try_new(unit).unwrap());
        let empty = rune::to_value(Object::new()).unwrap();
        let err = vm.call(["check"], (empty,)).unwrap_err();

        let bare = render_vm_error(&err, None);
        assert_eq!(bare, err.to_string());
        assert!(!bare.contains("session_id"), "{bare}");
        assert!(!bare.contains("Backtrace"), "{bare}");

        let rich = render_vm_error(&err, Some(&sources));
        assert!(rich.contains("session_id"), "{rich}");
        assert!(rich.contains("Backtrace"), "{rich}");
        assert!(rich.contains("inputs.session_id"), "{rich}");
    }
}
