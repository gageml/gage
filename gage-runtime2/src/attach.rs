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
//! `Files::include(patterns)` names the include patterns, `.root(path)`
//! the directory to select under, and `.exclude(patterns)` the exclude
//! patterns. Patterns are one string or a list of strings. The root defaults to the scanner's directory, a relative
//! root is under that directory, and `~` is expanded.
//! `files.key_part()` is the part of a key the selection contributes.
//!
//! The writer's `.name(str)`, `.key(key)`, `.target(object)`, and
//! `.targets(list)` are optional. The key is the identity a later
//! attach addresses: a named attach without one takes the name and
//! the selection's key part. A target is a `Session`, an object id or
//! prefix, or a Gage URL; targets given to an attachment that exists
//! are added to those it has.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use datafusion::prelude::SessionContext;
use gage_query2::{ContextBuilder, ScanScope};
use gage_runtime::error::Error;
use gage_session::Driver;
use gage_store::{
    AttachmentLinkOutcome, AttachmentSpec, AttachmentStore, DatasetStore, Store, selection_key_part,
};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};
use tokio::sync::{OnceCell, mpsc};

use crate::key::encode_key;
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
    /// The directory holding the scanner's source; the default root
    pub scanner_dir: PathBuf,
    pub store: Arc<tokio::sync::Mutex<Store>>,
    /// Every attachment the function writes is reported here
    pub attached: mpsc::UnboundedSender<Attached>,
    /// The driver that reopens a session's native source for
    /// `session.native()`
    pub driver: Arc<dyn Driver>,
    query_store: Arc<Mutex<Store>>,
    dataset_query: Arc<OnceCell<SessionContext>>,
}

impl AttachContext {
    /// Open the context over the store at `store_path`: one handle for
    /// the writes and one for the query context.
    pub fn new(
        dataset: ScanDatasetRef,
        scanner: String,
        scanner_dir: PathBuf,
        store_path: &Path,
        attached: mpsc::UnboundedSender<Attached>,
        driver: Arc<dyn Driver>,
    ) -> Result<Self, gage_store::StoreError> {
        Ok(AttachContext {
            dataset,
            scanner,
            scanner_dir,
            store: Arc::new(tokio::sync::Mutex::new(Store::open(store_path)?)),
            attached,
            driver,
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

/// One attachment an attachment function wrote: the object, its name,
/// key, and targets, and what the dataset link did.
#[derive(Debug, Clone, PartialEq, Eq, Any)]
#[rune(item = ::gage)]
pub struct Attached {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: Option<String>,
    #[rune(get)]
    pub key: Option<String>,
    /// The targets given to this write, as Gage URLs
    #[rune(skip)]
    pub targets: Vec<String>,
    /// `added`, `updated`, or `unchanged`
    #[rune(get)]
    pub outcome: String,
}

impl Attached {
    /// The targets given to this write, as Gage URLs.
    #[rune::function(instance)]
    fn targets(&self) -> Vec<String> {
        self.targets.clone()
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Attached {{ id: {:?}, name: {:?}, key: {:?}, targets: {:?}, outcome: {:?} }}",
            self.id, self.name, self.key, self.targets, self.outcome
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
    m.function_meta(Files::key_part)?;
    m.function_meta(Files::debug)?;
    m.ty::<AttachWriter>()?;
    m.function_meta(AttachWriter::name)?;
    m.function_meta(AttachWriter::key)?;
    m.function_meta(AttachWriter::target)?;
    m.function_meta(AttachWriter::targets__meta)?;
    m.associated_function(&Protocol::INTO_FUTURE, |w: AttachWriter| async move {
        write(w).await
    })?;
    m.ty::<Attached>()?;
    m.function_meta(Attached::targets)?;
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
        key: None,
        targets: Vec::new(),
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
    /// A selection of the files `patterns` match, relative to the
    /// root. One pattern as a string, or a list of them.
    #[rune::function(keep, path = Self::include)]
    fn include(patterns: Value) -> Result<Files, VmError> {
        Ok(Files {
            includes: string_list(&patterns)?,
            excludes: Vec::new(),
            root: None,
        })
    }

    /// Leave out the files `patterns` match: one string, or a list.
    #[rune::function(instance)]
    fn exclude(mut self, patterns: Value) -> Result<Files, VmError> {
        self.excludes = string_list(&patterns)?;
        Ok(self)
    }

    /// Select under `path` instead of the scanner's directory. A
    /// relative path is under the scanner's directory; a leading `~`
    /// is the home directory.
    #[rune::function(instance)]
    fn root(mut self, path: &str) -> Files {
        self.root = Some(path.to_string());
        self
    }

    /// The part of an attachment key this selection contributes: a
    /// stable token for the root and the patterns, for a scanner that
    /// composes its own key. The error is a root that does not
    /// resolve.
    #[rune::function(instance)]
    fn key_part(&self) -> Result<String, Error> {
        let root = self.canonical_root()?;
        Ok(selection_key_part(&root, &self.includes, &self.excludes))
    }

    /// The root as the store records it: `~` expanded, resolved
    /// against the scanner's directory, canonicalized. Outside an
    /// attachment function the base is the current directory.
    fn canonical_root(&self) -> Result<PathBuf, Error> {
        let base = match ATTACH_CTX.try_with(|ctx| ctx.scanner_dir.clone()) {
            Ok(dir) => dir,
            Err(_outside_scope) => std::env::current_dir()
                .map_err(|e| Error::Args(format!("attach: current directory: {e}")))?,
        };
        let root = match &self.root {
            Some(path) => base.join(expand_home(path)),
            None => base,
        };
        root.canonicalize()
            .map_err(|e| Error::Args(format!("attach: root {}: {e}", root.display())))
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

/// One string as a one-element list, or the strings of a Rune list,
/// borrowed.
fn string_list(v: &Value) -> Result<Vec<String>, VmError> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(vec![s.to_string()]);
    }
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
    /// The key as given; encoded at the await
    #[rune(skip)]
    key: Option<Value>,
    #[rune(skip)]
    targets: Vec<Value>,
}

impl AttachWriter {
    /// Name the attachment, so scanners can select it.
    #[rune::function(instance)]
    fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// The identity a later attach addresses, in place of the default
    /// built from the name and `files.key_part()`. A string, or a
    /// tuple of strings and integers.
    #[rune::function(instance)]
    fn key(mut self, key: Value) -> Self {
        self.key = Some(key);
        self
    }

    /// An object the files are about: a `Session`, an object id or
    /// prefix, or a Gage URL. Repeatable.
    #[rune::function(instance)]
    fn target(mut self, target: Value) -> Self {
        self.targets.push(target);
        self
    }

    /// Objects the files are about, as a list of what `target` takes.
    #[rune::function(keep, instance)]
    fn targets(mut self, list: Value) -> Result<Self, VmError> {
        let items = list.borrow_ref::<rune::runtime::Vec>()?;
        self.targets.extend(items.iter().cloned());
        Ok(self)
    }
}

/// Select the files, write the object, and link it into the dataset.
/// The inner error is the scanner's: a root that is not a directory,
/// a bad pattern, a bad target, a cap exceeded; the outer error is a
/// runtime fault.
async fn write(w: AttachWriter) -> Result<Result<Attached, Error>, VmError> {
    let ctx = current()?;
    let root = match w.files.canonical_root() {
        Ok(root) => root,
        Err(e) => return Ok(Err(e)),
    };
    let key = match w.key.as_ref().map(encode_key).transpose() {
        Ok(key) => key,
        Err(e) => return Ok(Err(e)),
    };
    let mut targets = Vec::with_capacity(w.targets.len());
    for value in &w.targets {
        match target_url(value).await? {
            Ok(url) => targets.push(url),
            Err(e) => return Ok(Err(e)),
        }
    }
    let store = ctx.store.lock().await;
    let spec = AttachmentSpec {
        name: w.name.as_deref(),
        key: key.as_deref(),
        targets: &targets,
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
        key: added.key,
        targets,
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
    fn patterns_are_one_string_or_a_list() {
        let one = rune::to_value("a.txt").unwrap();
        assert_eq!(string_list(&one).unwrap(), ["a.txt"]);
        let list = rune::to_value(vec!["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(string_list(&list).unwrap(), ["a", "b"]);
        assert!(string_list(&rune::to_value(1i64).unwrap()).is_err());
        assert_eq!(&*one.borrow_string_ref().unwrap(), "a.txt");
    }

    #[test]
    fn targets_leaves_the_caller_list_readable() {
        let session = rune::to_value(crate::scan::Session {
            id: "s1".to_string(),
            line_count: 0,
            commit: String::new(),
        })
        .unwrap();
        let list = rune::to_value(vec![session.clone()]).unwrap();
        let writer = AttachWriter {
            files: Files {
                includes: vec!["x".into()],
                excludes: Vec::new(),
                root: None,
            },
            name: None,
            key: None,
            targets: Vec::new(),
        };
        let writer = writer.targets(list.clone()).unwrap();
        assert_eq!(writer.targets.len(), 1);
        let items = list.borrow_ref::<rune::runtime::Vec>().unwrap();
        assert_eq!(items.len(), 1);
        let s = session.borrow_ref::<crate::scan::Session>().unwrap();
        assert_eq!(s.id, "s1");
    }

    #[test]
    fn home_expands_only_a_leading_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/.claude"), home.join(".claude"));
        assert_eq!(expand_home("/tmp/~x"), PathBuf::from("/tmp/~x"));
        assert_eq!(expand_home("rel/~"), PathBuf::from("rel/~"));
    }
}
