//! `scan()`: the running scan and its sessions, for scanners.
//!
//! `scan()` returns the [`Scan`]: its id and its dataset.
//! `scan().sessions()` is a [`SessionsQuery`]; awaiting it reads the
//! dataset's members at the commit the scan links and yields a
//! [`Sessions`] iterator of [`Session`] values. The order is the
//! store's read order, which scanners must not rely on. Every task
//! runs under a [`ScanContext`], scoped by the orchestrator through
//! [`SCAN_CTX`]; `scan()` outside one is a VM error.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{
    Array, BooleanArray, Int64Array, StringArray, TimestampMillisecondArray,
};
use datafusion::prelude::SessionContext;
use gage_query2::ContextBuilder;
use gage_query2::scope::SessionScope;
use gage_runtime::datetime::{self, DateTime};
use gage_store::Store;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, VmError};
use rune::{Any, ContextError, Module};
use tokio::sync::OnceCell;

tokio::task_local! {
    /// The running task's scan, read by [`scan`]
    pub static SCAN_CTX: ScanContext;
}

/// The scan a task runs under: its id, its dataset, and the store
/// the dataset is read from. The store is shared under a mutex
/// because its git reader is single-threaded. The query context over
/// the dataset's members is built on first use and shared by every
/// task, so a session's rows are derived once per scan.
#[derive(Clone)]
pub struct ScanContext {
    pub scan_id: String,
    /// `None` when the scan has no dataset; `sessions()` is then empty
    pub dataset: Option<ScanDatasetRef>,
    pub store: Arc<Mutex<Store>>,
    /// The staging directory the scan's notes are written under,
    /// `staging/<scan_id>/notes/`
    pub notes_dir: PathBuf,
    query: Arc<OnceCell<(Arc<SessionScope>, SessionContext)>>,
}

impl ScanContext {
    pub fn new(
        scan_id: String,
        dataset: Option<ScanDatasetRef>,
        store: Arc<Mutex<Store>>,
        notes_dir: PathBuf,
    ) -> Self {
        ScanContext {
            scan_id,
            dataset,
            store,
            notes_dir,
            query: Arc::new(OnceCell::new()),
        }
    }

    /// The scope and query context over the dataset's members at the
    /// commit the scan links, built on the first read of any kind.
    /// Without a dataset the scope is empty.
    async fn scoped(&self) -> Result<&(Arc<SessionScope>, SessionContext), VmError> {
        self.query
            .get_or_try_init(|| async {
                let scope = match &self.dataset {
                    Some(dataset) => {
                        SessionScope::for_dataset(Arc::clone(&self.store), &dataset.commit_sha)
                            .map_err(|e| {
                                VmError::panic(format!("read dataset {}: {e}", dataset.id))
                            })?
                    }
                    None => SessionScope::with_sessions(Arc::clone(&self.store), Vec::new()),
                };
                let scope = Arc::new(scope);
                let ctx = ContextBuilder::new(Some(Arc::clone(&self.store)))
                    .scope(Arc::clone(&scope))
                    .build()
                    .await;
                Ok((scope, ctx))
            })
            .await
    }

    /// The query context over the dataset's members.
    pub(crate) async fn query_context(&self) -> Result<&SessionContext, VmError> {
        Ok(&self.scoped().await?.1)
    }

    /// The commit the scan reads for the member session `session_id`,
    /// or `None` when it is not a member.
    pub(crate) async fn member_commit(&self, session_id: &str) -> Result<Option<String>, VmError> {
        let (scope, _) = self.scoped().await?;
        let members = scope
            .sessions(&[])
            .map_err(|e| VmError::panic(format!("scan members: {e}")))?;
        Ok(members
            .into_iter()
            .find(|r| r.id == session_id)
            .map(|r| r.commit))
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
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionsQuery| async move {
        fetch_sessions(q).await
    })?;
    m.ty::<Session>()?;
    m.function_meta(Session::attrs)?;
    m.function_meta(Session::debug)?;
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

/// The running scan.
fn scan() -> Result<Scan, VmError> {
    let ctx = current()?;
    Ok(Scan {
        id: ctx.scan_id,
        dataset: ctx.dataset.map(|d| ScanDataset { id: d.id }),
    })
}

pub(crate) fn current() -> Result<ScanContext, VmError> {
    SCAN_CTX
        .try_with(|ctx| ctx.clone())
        .map_err(|_outside_scope| {
            VmError::panic("scan() is available only inside a running scan task")
        })
}

/// The running scan, as a scanner sees it.
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
        SessionsQuery
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
pub struct SessionsQuery;

/// The scan's sessions, read from the scoped `session` table, whose
/// rows are the members in member order. Only the id and the version
/// are held per session; `attrs()` reads the rest on request.
async fn fetch_sessions(_query: SessionsQuery) -> Result<Sessions, VmError> {
    const SQL: &str = "SELECT id, locator FROM session";
    let ctx = current()?;
    let batches = run(ctx.query_context().await?, SQL).await?;
    let mut items = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let locators = string_column(batch, 1);
        for i in 0..batch.num_rows() {
            let locator = locators.value(i);
            let commit = locator
                .strip_prefix("git:")
                .unwrap_or_else(|| panic!("session locator is git:<sha>, got {locator:?}"));
            items.push(Session {
                id: ids.value(i).to_string(),
                commit: commit.to_string(),
            });
        }
    }
    Ok(Sessions::new(items))
}

/// Run `sql` on the scan's query context.
async fn run(
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

fn string_column(batch: &datafusion::arrow::record_batch::RecordBatch, i: usize) -> &StringArray {
    batch
        .column(i)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("session table column types are fixed")
}

/// A stored session, as a scanner sees it: its id and, held for the
/// runtime, the version the scan reads. Everything else is read on
/// request through [`Session::attrs`].
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Session {
    /// The Gage object id
    #[rune(get)]
    pub id: String,
    #[rune(skip)]
    pub commit: String,
}

impl Session {
    /// The session's attributes, read when awaited.
    #[rune::function(instance)]
    fn attrs(&self) -> SessionAttrsQuery {
        SessionAttrsQuery {
            id: self.id.clone(),
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "Session {{ id: {:?} }}", self.id)?;
        Ok(())
    }
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
}

impl SessionAttrs {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "SessionAttrs {{ id: {:?}, project: {:?}, modified: {}, created: {}, \
             native_mtime: {}, native_size: {}, native_id: {:?}, native_source: {:?}, \
             session_type: {:?}, driver: {:?}, title: {:?}, model: {:?}, \
             message_count: {:?}, is_empty: {} }}",
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
            self.is_empty
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
                native_source, session_type, driver, title, model, message_count, is_empty \
         FROM session WHERE id = '{}'",
        q.id.replace('\'', "''")
    );
    let ctx = current()?;
    let batches = run(ctx.query_context().await?, &sql).await?;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        return Err(VmError::panic(format!(
            "session {} is not a member of the scan",
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
    })
}

/// A double-ended, exact-size iterator over a scan's sessions, in
/// dataset order.
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
            commit: format!("commit-{id}"),
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
                .contains("scan() is available only inside a running scan task"),
            "{err}"
        );
    }
}
