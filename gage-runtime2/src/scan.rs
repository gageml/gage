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

use datafusion::arrow::array::{Array, StringArray, TimestampMillisecondArray};
use datafusion::prelude::SessionContext;
use gage_query2::ContextBuilder;
use gage_query2::scope::SessionScope;
use gage_runtime::datetime::{self, DateTime};
use gage_store::Store;
use rune::runtime::{Protocol, VmError};
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
    m.ty::<ScanDataset>()?;
    m.ty::<SessionsQuery>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionsQuery| async move {
        fetch_sessions(q).await
    })?;
    m.ty::<Session>()?;
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
}

/// The dataset a scan links.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct ScanDataset {
    #[rune(get)]
    pub id: String,
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
/// rows are the members in member order.
async fn fetch_sessions(_query: SessionsQuery) -> Result<Sessions, VmError> {
    const SQL: &str =
        "SELECT id, native_id, session_type, project, title, native_mtime FROM session";
    let ctx = current()?;
    let df_ctx = ctx.query_context().await?;
    let fail = |e: datafusion::error::DataFusionError| VmError::panic(format!("{SQL}: {e}"));
    let batches = df_ctx
        .sql(SQL)
        .await
        .map_err(fail)?
        .collect()
        .await
        .map_err(fail)?;
    let mut items = Vec::new();
    for batch in &batches {
        let col = |i: usize| {
            batch
                .column(i)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("session table column types are fixed")
        };
        let (ids, native_ids, types, projects, titles) = (col(0), col(1), col(2), col(3), col(4));
        let mtimes = batch
            .column(5)
            .as_any()
            .downcast_ref::<TimestampMillisecondArray>()
            .expect("native_mtime is a timestamp column");
        for i in 0..batch.num_rows() {
            let optional = |arr: &StringArray| arr.is_valid(i).then(|| arr.value(i).to_string());
            items.push(Session {
                id: ids.value(i).to_string(),
                native_id: native_ids.value(i).to_string(),
                session_type: types.value(i).to_string(),
                project: optional(projects),
                title: optional(titles),
                mtime: DateTime::from_millis(mtimes.value(i)),
            });
        }
    }
    Ok(Sessions::new(items))
}

/// A stored session, as a scanner sees it.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Session {
    /// The Gage object id
    #[rune(get)]
    pub id: String,
    /// The id the harness gave the session
    #[rune(get)]
    pub native_id: String,
    #[rune(get)]
    pub session_type: String,
    /// The project, as the driver names it
    #[rune(get)]
    pub project: Option<String>,
    #[rune(get)]
    pub title: Option<String>,
    /// When the native artifact was last touched at its source
    #[rune(get)]
    pub mtime: DateTime,
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
            native_id: format!("native-{id}"),
            session_type: "fake".into(),
            project: None,
            title: Some(format!("Title {id}")),
            mtime: DateTime::from_millis(1_000),
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
    fn session_getters_expose_the_record() {
        let mut vm = vm(r#"
            pub fn check(s) {
                (s.id, s.native_id, s.session_type, s.project, s.title, s.mtime.millis())
            }
            "#);
        let output = vm.call(["check"], (session("a"),)).unwrap();
        let fields: (String, String, String, Option<String>, Option<String>, i64) =
            rune::from_value(output).unwrap();
        assert_eq!(
            fields,
            (
                "a".into(),
                "native-a".into(),
                "fake".into(),
                None,
                Some("Title a".into()),
                1_000
            )
        );
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
