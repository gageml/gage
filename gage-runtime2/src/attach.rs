//! `dataset()` and `attach()`: the attach context, for the attachment
//! functions a scanner declares under `SCANNER.attachments`.
//!
//! An attachment function runs against a dataset, not a scan. It runs
//! under an [`AttachContext`], scoped by the orchestrator through
//! [`ATTACH_CTX`]. `dataset()` returns the [`Dataset`]; its
//! `sessions()` is the session query a scan has. `attach(files)`
//! returns an [`AttachWriter`]; awaiting it selects the files, writes
//! the attachment object, links it into the dataset, and yields the
//! [`Attached`] outcome. `scan()` is not available here.
//!
//! [`Files`] is the selection, mirroring `gage attachment add`:
//! `Files::include(list)` names the include patterns, `.root(path)`
//! the directory to select under (default the current directory, `~`
//! expanded), and `.exclude(list)` the exclude patterns. The writer's
//! `.name(str)` and `.target(object)` are optional; a target is a
//! `Session`, an object id or prefix, or a Gage URL.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use datafusion::prelude::SessionContext;
use gage_query2::{ContextBuilder, ScanScope};
use gage_runtime::error::Error;
use gage_store::{AttachmentLinkOutcome, AttachmentSpec, AttachmentStore, DatasetStore, Store};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};
use tokio::sync::{OnceCell, mpsc};

use crate::scan::{ScanDatasetRef, SessionsQuery, target_url};

tokio::task_local! {
    /// The running attachment function's context, read by [`dataset`]
    /// and [`attach`]
    pub static ATTACH_CTX: AttachContext;
}

/// The dataset an attachment function runs against, the scanner it
/// belongs to, and the store it writes. The query context over the
/// dataset's members is built on first use.
#[derive(Clone)]
pub struct AttachContext {
    pub dataset: ScanDatasetRef,
    pub scanner: String,
    pub store: Arc<tokio::sync::Mutex<Store>>,
    /// Every attachment the function writes is reported here
    pub attached: mpsc::UnboundedSender<Attached>,
    query_store: Arc<Mutex<Store>>,
    dataset_query: Arc<OnceCell<SessionContext>>,
}

impl AttachContext {
    /// Open the context over the store at `store_path`: one handle for
    /// the writes and one for the query context.
    pub fn new(
        dataset: ScanDatasetRef,
        scanner: String,
        store_path: &Path,
        attached: mpsc::UnboundedSender<Attached>,
    ) -> Result<Self, gage_store::StoreError> {
        Ok(AttachContext {
            dataset,
            scanner,
            store: Arc::new(tokio::sync::Mutex::new(Store::open(store_path)?)),
            attached,
            query_store: Arc::new(Mutex::new(Store::open(store_path)?)),
            dataset_query: Arc::new(OnceCell::new()),
        })
    }

    /// The dataset-scoped query context: the members and attachments
    /// at the dataset commit the context was opened at.
    pub(crate) async fn dataset_context(&self) -> Result<&SessionContext, VmError> {
        self.dataset_query
            .get_or_try_init(|| async {
                let ctx = ContextBuilder::new(Some(Arc::clone(&self.query_store)))
                    .scope(ScanScope::dataset(&self.dataset.commit_sha))
                    .build()
                    .await;
                tracing::info!("dataset query context built");
                Ok(ctx)
            })
            .await
    }
}

/// One attachment an attachment function wrote: the object, its name
/// and target, and what the dataset link did.
#[derive(Debug, Clone, PartialEq, Eq, Any)]
#[rune(item = ::gage)]
pub struct Attached {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: Option<String>,
    /// The target as a Gage URL
    #[rune(get)]
    pub target: Option<String>,
    /// `added`, `updated`, or `unchanged`
    #[rune(get)]
    pub outcome: String,
}

impl Attached {
    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Attached {{ id: {:?}, name: {:?}, target: {:?}, outcome: {:?} }}",
            self.id, self.name, self.target, self.outcome
        )?;
        Ok(())
    }
}

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("dataset", dataset).build()?;
    m.function("attach", attach).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<Dataset>()?;
    m.function_meta(Dataset::sessions)?;
    m.function_meta(Dataset::debug)?;
    m.ty::<Files>()?;
    m.function_meta(Files::include__meta)?;
    m.function_meta(Files::exclude)?;
    m.function_meta(Files::root)?;
    m.function_meta(Files::debug)?;
    m.ty::<AttachWriter>()?;
    m.function_meta(AttachWriter::name)?;
    m.function_meta(AttachWriter::target)?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: AttachWriter| async move {
        write(w).await
    })?;
    m.ty::<Attached>()?;
    m.function_meta(Attached::debug)?;
    Ok(m)
}

/// The dataset the attachment function runs against.
fn dataset() -> Result<Dataset, VmError> {
    let ctx = current()?;
    Ok(Dataset { id: ctx.dataset.id })
}

/// Start an attachment of `files`; awaiting the writer runs it.
fn attach(files: Ref<Files>) -> AttachWriter {
    AttachWriter {
        files: files.clone(),
        name: None,
        target: None,
    }
}

pub(crate) fn current() -> Result<AttachContext, VmError> {
    ATTACH_CTX
        .try_with(|ctx| ctx.clone())
        .map_err(|_outside_scope| {
            VmError::panic(
                "dataset() and attach() are available only inside an attachment function",
            )
        })
}

/// The dataset, as an attachment function sees it.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Dataset {
    #[rune(get)]
    pub id: String,
}

impl Dataset {
    /// The dataset's sessions, read when awaited.
    #[rune::function(instance)]
    fn sessions(&self) -> SessionsQuery {
        SessionsQuery {
            newest_first: false,
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "Dataset {{ id: {:?} }}", self.id)?;
        Ok(())
    }
}

/// A file selection: include patterns, exclude patterns, and the root
/// they are relative to.
#[derive(Any, Clone, Debug)]
#[rune(item = ::gage)]
pub struct Files {
    #[rune(skip)]
    includes: Vec<String>,
    #[rune(skip)]
    excludes: Vec<String>,
    #[rune(skip)]
    root: Option<String>,
}

impl Files {
    /// A selection of the files `patterns` match, relative to the root.
    #[rune::function(keep, path = Self::include)]
    fn include(patterns: Value) -> Result<Files, VmError> {
        Ok(Files {
            includes: string_list(&patterns)?,
            excludes: Vec::new(),
            root: None,
        })
    }

    /// Leave out the files `patterns` match.
    #[rune::function(instance)]
    fn exclude(mut self, patterns: Value) -> Result<Files, VmError> {
        self.excludes = string_list(&patterns)?;
        Ok(self)
    }

    /// Select under `path` instead of the current directory. A leading
    /// `~` is the home directory.
    #[rune::function(instance)]
    fn root(mut self, path: &str) -> Files {
        self.root = Some(path.to_string());
        self
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Files {{ includes: {:?}, excludes: {:?}, root: {:?} }}",
            self.includes, self.excludes, self.root
        )?;
        Ok(())
    }
}

/// The strings of a Rune list, borrowed.
fn string_list(v: &Value) -> Result<Vec<String>, VmError> {
    let list = v.borrow_ref::<rune::runtime::Vec>()?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        out.push(item.borrow_string_ref()?.to_string());
    }
    Ok(out)
}

/// The value of `attach(files)`. Awaiting it writes the attachment.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachWriter {
    #[rune(skip)]
    files: Files,
    #[rune(skip)]
    name: Option<String>,
    #[rune(skip)]
    target: Option<Value>,
}

impl AttachWriter {
    /// Name the attachment, so scanners can select it.
    #[rune::function(instance)]
    fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// The object the files are about: a `Session`, an object id or
    /// prefix, or a Gage URL.
    #[rune::function(instance)]
    fn target(mut self, target: Value) -> Self {
        self.target = Some(target);
        self
    }
}

/// Select the files, write the object, and link it into the dataset.
/// The inner error is the scanner's: a root that is not a directory,
/// a bad pattern, a bad target, a cap exceeded; the outer error is a
/// runtime fault.
async fn write(w: AttachWriter) -> Result<Result<Attached, Error>, VmError> {
    let ctx = current()?;
    let root = match &w.files.root {
        Some(path) => expand_home(path),
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => {
                return Ok(Err(Error::Args(format!("attach: current directory: {e}"))));
            }
        },
    };
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(e) => {
            return Ok(Err(Error::Args(format!(
                "attach: root {}: {e}",
                root.display()
            ))));
        }
    };
    let target = match &w.target {
        Some(value) => match target_url(value).await? {
            Ok(url) => Some(url),
            Err(e) => return Ok(Err(e)),
        },
        None => None,
    };
    let store = ctx.store.lock().await;
    let spec = AttachmentSpec {
        name: w.name.as_deref(),
        target: target.as_deref(),
        root: &root,
        includes: &w.files.includes,
        excludes: &w.files.excludes,
    };
    let added = match AttachmentStore::from(&*store).add(&spec) {
        Ok(added) => added,
        Err(e) => return Ok(Err(Error::Args(format!("attach: {e}")))),
    };
    let linked = match DatasetStore::from(&*store)
        .attachments_link(&ctx.dataset.id, std::slice::from_ref(&added.id))
    {
        Ok(linked) => linked,
        Err(e) => return Ok(Err(Error::Db(format!("attach: {e}")))),
    };
    drop(store);
    let outcome = match linked.first().map(|o| &o.outcome) {
        Some(AttachmentLinkOutcome::Linked) => "added",
        Some(AttachmentLinkOutcome::Updated) => "updated",
        Some(AttachmentLinkOutcome::Unchanged) | None => "unchanged",
    };
    let attached = Attached {
        id: added.id,
        name: w.name,
        target,
        outcome: outcome.to_string(),
    };
    match ctx.attached.send(attached.clone()) {
        Ok(()) => {}
        // The orchestrator dropped the receiver: it stopped listening
        // and the report has nowhere to go
        Err(mpsc::error::SendError(_)) => {
            tracing::warn!(id = %attached.id, "attach report dropped")
        }
    }
    Ok(Ok(attached))
}

/// `~` and `~/...` under `HOME`; any other path as given.
fn expand_home(path: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    match path.strip_prefix("~/") {
        Some(rest) => home().map_or_else(|| PathBuf::from(path), |h| h.join(rest)),
        None if path == "~" => home().unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_expands_only_a_leading_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/.claude"), home.join(".claude"));
        assert_eq!(expand_home("/tmp/~x"), PathBuf::from("/tmp/~x"));
        assert_eq!(expand_home("rel/~"), PathBuf::from("rel/~"));
    }
}
