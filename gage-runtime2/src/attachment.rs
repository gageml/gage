//! `scan().attachments()` and `session.attachments()`: the
//! attachments the scan's dataset holds, for scanners.
//!
//! `scan().attachments()` is an [`AttachmentsQuery`], and
//! `session.attachments()` the same with the session as target. It
//! takes no argument, so its await is the [`Attachments`] iterator
//! itself. `.name(pattern)` keeps the attachments whose name matches
//! a `*`-glob, as a task's `wants` does, `.names([...])` those
//! matching any of several, and `.target(object)` those with the
//! object among their targets, given as a `Session`, an id or
//! prefix, or a Gage URL. Each moves the chain to a
//! [`FilteredAttachmentsQuery`], whose await is a `Result`: a
//! pattern and a target string are syntaxes, and the scanner handles
//! a bad one. `.hwm(key)` and `.unseen(key)` on either query read the
//! attachments' watermarks under `key`; see `crate::validate`.
//! A read yields [`Attachment`] values in dataset order. A name is a
//! selector, not an identifier, so there is no lookup of one
//! attachment by name.
//!
//! An attachment offers `files()`, awaited to the list of file paths,
//! and `file(path)`, awaited to `Some(AttachmentFile)` or `None`, both
//! read directly from the store at the attachment's linked commit,
//! bypassing the `attachment_file` SQL projection so the raw bytes
//! round-trip unchanged. A file holds its bytes; `bytes()` returns
//! them, and `text()` and `json()` are fallible, returning
//! `Error::Decode` for content that is not UTF-8 or not JSON.
//! `digest` is the content digest of the files, the version a
//! watermark or a note records for an attachment.

use datafusion::arrow::array::{Array, ListArray, StringArray};
use gage_runtime::error::Error;
use gage_runtime::value::json_to_value;
use gage_store::AttachmentStore;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Bytes, Formatter, Protocol, Ref, Value, Vec as RuneVec, VmError};
use rune::{Any, ContextError, Module};

use crate::note::{name_predicate, patterns};
use crate::scan::{
    Scan, Session, dataset_query, run, sql_str, store_handle, string_column, target_url,
};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.function_meta(Scan::attachments)?;
    m.function_meta(Session::attachments)?;
    m.ty::<AttachmentsQuery>()?;
    m.function_meta(AttachmentsQuery::name)?;
    m.function_meta(AttachmentsQuery::names)?;
    m.function_meta(AttachmentsQuery::target)?;
    m.function_meta(crate::validate::attachments_hwm)?;
    m.function_meta(crate::validate::attachments_unseen)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: AttachmentsQuery| async move {
        fetch_attachments(q).await
    })?;
    m.ty::<FilteredAttachmentsQuery>()?;
    m.function_meta(FilteredAttachmentsQuery::name)?;
    m.function_meta(FilteredAttachmentsQuery::names)?;
    m.function_meta(FilteredAttachmentsQuery::target)?;
    m.function_meta(crate::validate::filtered_attachments_hwm)?;
    m.function_meta(crate::validate::filtered_attachments_unseen)?;
    m.associated_function(
        &Protocol::INTO_FUTURE,
        |q: FilteredAttachmentsQuery| async move { fetch_filtered_attachments(q).await },
    )?;
    m.ty::<Attachment>()?;
    m.function_meta(Attachment::files)?;
    m.function_meta(Attachment::file)?;
    m.function_meta(Attachment::debug)?;
    m.ty::<AttachmentFilesQuery>()?;
    m.associated_function(
        &Protocol::INTO_FUTURE,
        |q: AttachmentFilesQuery| async move { fetch_files(q).await },
    )?;
    m.ty::<AttachmentFileQuery>()?;
    m.associated_function(
        &Protocol::INTO_FUTURE,
        |q: AttachmentFileQuery| async move { fetch_file(q).await },
    )?;
    m.ty::<AttachmentFile>()?;
    m.function_meta(AttachmentFile::bytes)?;
    m.function_meta(AttachmentFile::text)?;
    m.function_meta(AttachmentFile::json)?;
    m.function_meta(AttachmentFile::debug)?;
    m.ty::<Attachments>()?;
    m.function_meta(Attachments::next__meta)?;
    m.function_meta(Attachments::nth__meta)?;
    m.function_meta(Attachments::size_hint__meta)?;
    m.function_meta(Attachments::len__meta)?;
    m.function_meta(Attachments::next_back__meta)?;
    m.implement_trait::<Attachments>(rune::item!(::std::iter::Iterator))?;
    m.implement_trait::<Attachments>(rune::item!(::std::iter::DoubleEndedIterator))?;
    m.implement_trait::<Attachments>(rune::item!(::std::iter::ExactSizeIterator))?;
    Ok(m)
}

impl Scan {
    /// The attachments the scan's dataset holds, read when awaited.
    #[rune::function(instance)]
    fn attachments(&self) -> AttachmentsQuery {
        AttachmentsQuery { target_url: None }
    }
}

impl Session {
    /// The attachments about this session, read when awaited.
    #[rune::function(instance)]
    fn attachments(&self) -> AttachmentsQuery {
        AttachmentsQuery {
            target_url: Some(format!("session:{}", self.id)),
        }
    }
}

/// The value of `scan().attachments()` and `session.attachments()`.
/// It has no argument the scanner wrote, so its await is the
/// [`Attachments`] iterator itself.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct AttachmentsQuery {
    /// The session's URL for `session.attachments()`, known at
    /// construction and needing no lookup
    #[rune(skip)]
    target_url: Option<String>,
}

/// `scan().attachments()` narrowed by name or target. Its await is a
/// `Result`: a pattern and a target string are syntaxes.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct FilteredAttachmentsQuery {
    #[rune(skip)]
    pub(crate) select: AttachmentsSelect,
}

/// What an attachments read selects, shared by both query types.
#[derive(Clone)]
pub(crate) struct AttachmentsSelect {
    names: Option<Vec<String>>,
    target: Option<Target>,
}

/// A target filter, resolved to a URL when the query runs.
#[derive(Debug, Clone)]
enum Target {
    Url(String),
    Value(Value),
}

impl AttachmentsQuery {
    /// The selection as a filtered query would hold it.
    pub(crate) fn select(&self) -> AttachmentsSelect {
        AttachmentsSelect {
            names: None,
            target: self.target_url.clone().map(Target::Url),
        }
    }

    /// Keep the attachments whose name matches `pattern`: `*` matches
    /// any run of characters; a pattern without `*` is an exact name.
    #[rune::function(instance)]
    fn name(self, pattern: &str) -> FilteredAttachmentsQuery {
        FilteredAttachmentsQuery {
            select: self.select().with_name(pattern),
        }
    }

    /// Keep the attachments whose name matches any of `list`, each an
    /// exact name or a `*` pattern.
    #[rune::function(instance)]
    fn names(self, list: Ref<RuneVec>) -> Result<FilteredAttachmentsQuery, VmError> {
        Ok(FilteredAttachmentsQuery {
            select: self.select().with_names(&list)?,
        })
    }

    /// Keep the attachments with `object` among their targets: a
    /// `Session`, an object id or prefix, or a Gage URL.
    #[rune::function(instance)]
    fn target(self, object: Value) -> FilteredAttachmentsQuery {
        FilteredAttachmentsQuery {
            select: self.select().with_target(object),
        }
    }
}

impl FilteredAttachmentsQuery {
    /// As [`AttachmentsQuery::name`], replacing the patterns so far.
    #[rune::function(instance)]
    fn name(self, pattern: &str) -> Self {
        FilteredAttachmentsQuery {
            select: self.select.with_name(pattern),
        }
    }

    /// As [`AttachmentsQuery::names`], replacing the patterns so far.
    #[rune::function(instance)]
    fn names(self, list: Ref<RuneVec>) -> Result<Self, VmError> {
        Ok(FilteredAttachmentsQuery {
            select: self.select.with_names(&list)?,
        })
    }

    /// As [`AttachmentsQuery::target`], replacing the target so far.
    #[rune::function(instance)]
    fn target(self, object: Value) -> Self {
        FilteredAttachmentsQuery {
            select: self.select.with_target(object),
        }
    }
}

impl AttachmentsSelect {
    fn with_name(mut self, pattern: &str) -> Self {
        self.names = Some(vec![pattern.to_string()]);
        self
    }

    fn with_names(mut self, list: &RuneVec) -> Result<Self, VmError> {
        self.names = Some(patterns(list)?);
        Ok(self)
    }

    fn with_target(mut self, object: Value) -> Self {
        self.target = Some(Target::Value(object));
        self
    }
}

/// The dataset's attachments, in dataset order. Without a dataset
/// there are none.
async fn fetch_attachments(q: AttachmentsQuery) -> Result<Attachments, VmError> {
    Ok(Attachments::new(
        attachments_matching(None, q.target_url.as_deref()).await?,
    ))
}

/// The dataset's attachments matching the filtered query. The inner
/// error is a target argument that names nothing.
async fn fetch_filtered_attachments(
    q: FilteredAttachmentsQuery,
) -> Result<Result<Attachments, Error>, VmError> {
    Ok(attachment_list(q.select).await?.map(Attachments::new))
}

/// The attachments a selection yields, as a list. The inner error is
/// a target argument that names nothing.
pub(crate) async fn attachment_list(
    select: AttachmentsSelect,
) -> Result<Result<Vec<Attachment>, Error>, VmError> {
    let target = match &select.target {
        Some(Target::Url(url)) => Some(url.clone()),
        Some(Target::Value(value)) => match target_url(value).await? {
            Ok(url) => Some(url),
            Err(e) => return Ok(Err(e)),
        },
        None => None,
    };
    Ok(Ok(attachments_matching(
        select.names.as_deref(),
        target.as_deref(),
    )
    .await?))
}

/// The attachments whose name matches any of `names` and whose
/// targets include `target`, each when given.
async fn attachments_matching(
    names: Option<&[String]>,
    target: Option<&str>,
) -> Result<Vec<Attachment>, VmError> {
    let mut clauses = Vec::new();
    if let Some(names) = names {
        clauses.push(format!("({})", name_predicate("name", names)));
    }
    if let Some(url) = target {
        clauses.push(format!("array_has(targets, '{}')", sql_str(url)));
    }
    let sql = if clauses.is_empty() {
        format!("SELECT {ATTACHMENT_COLUMNS} FROM attachment")
    } else {
        format!(
            "SELECT {ATTACHMENT_COLUMNS} FROM attachment WHERE {}",
            clauses.join(" AND ")
        )
    };
    attachments(&sql).await
}

/// Every attachment of the scan's dataset, in dataset order.
pub(crate) async fn scan_attachments() -> Result<Vec<Attachment>, VmError> {
    attachments(&format!("SELECT {ATTACHMENT_COLUMNS} FROM attachment")).await
}

const ATTACHMENT_COLUMNS: &str = "id, name, key, targets, root, commit, digest";

async fn attachments(sql: &str) -> Result<Vec<Attachment>, VmError> {
    let batches = run(&dataset_query().await?, sql).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let names = string_column(batch, 1);
        let keys = string_column(batch, 2);
        let targets = batch
            .column(3)
            .as_any()
            .downcast_ref::<ListArray>()
            .expect("attachment targets is a list column");
        let roots = string_column(batch, 4);
        let commits = string_column(batch, 5);
        let digests = string_column(batch, 6);
        let optional =
            |col: &StringArray, i: usize| col.is_valid(i).then(|| col.value(i).to_string());
        for i in 0..batch.num_rows() {
            let row_targets = targets.value(i);
            let row_targets = row_targets
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("attachment targets hold strings");
            let row_targets: Vec<String> = row_targets.iter().flatten().map(String::from).collect();
            out.push(Attachment {
                id: ids.value(i).to_string(),
                name: optional(names, i),
                key: optional(keys, i),
                targets: rune::to_value(row_targets).map_err(VmError::from)?,
                root: roots.value(i).to_string(),
                digest: optional(digests, i),
                commit: commits.value(i).to_string(),
            });
        }
    }
    Ok(out)
}

/// An attachment, as a scanner sees it.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Attachment {
    /// The Gage object id
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: Option<String>,
    /// The identity a later attach addresses
    #[rune(get)]
    pub key: Option<String>,
    /// The objects the files are about, a list of Gage URLs
    #[rune(get)]
    pub targets: Value,
    /// The directory the files were selected under
    #[rune(get)]
    pub root: String,
    /// The content digest of the files; `None` on an attachment
    /// written before the digest existed
    #[rune(get)]
    pub digest: Option<String>,
    /// The commit the scan reads
    #[rune(skip)]
    pub commit: String,
}

/// The attachment id in a scanner's argument: an [`Attachment`] or
/// an id string. The value is borrowed, not taken.
pub(crate) fn attachment_id(v: &Value) -> Result<String, VmError> {
    if let Ok(a) = v.borrow_ref::<Attachment>() {
        return Ok(a.id.clone());
    }
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(s.to_string());
    }
    Err(VmError::panic(format!(
        "expected an Attachment or attachment id string, got {}",
        v.type_info()
    )))
}

impl Attachment {
    /// The attachment's file paths, read when awaited.
    #[rune::function(instance)]
    fn files(&self) -> AttachmentFilesQuery {
        AttachmentFilesQuery {
            commit: self.commit.clone(),
        }
    }

    /// The file at `path`, read when awaited: `Some(AttachmentFile)`,
    /// or `None` when the attachment has no such file.
    #[rune::function(instance)]
    fn file(&self, path: &str) -> AttachmentFileQuery {
        AttachmentFileQuery {
            commit: self.commit.clone(),
            path: path.to_string(),
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "Attachment {{ id: {:?}, name: {:?}, key: {:?}, targets: {:?} }}",
            self.id, self.name, self.key, self.targets
        )?;
        Ok(())
    }
}

/// The value of `attachment.files()`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentFilesQuery {
    #[rune(skip)]
    commit: String,
}

async fn fetch_files(q: AttachmentFilesQuery) -> Result<Vec<String>, VmError> {
    let store = store_handle()?;
    let store = store.lock().await;
    let attachments = AttachmentStore::from(&*store);
    let files = attachments
        .files(&q.commit)
        .map_err(|e| VmError::panic(format!("attachment commit {}: {e}", q.commit)))?;
    Ok(files.into_iter().map(|f| f.path).collect())
}

/// The value of `attachment.file(path)`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentFileQuery {
    #[rune(skip)]
    commit: String,
    #[rune(skip)]
    path: String,
}

async fn fetch_file(q: AttachmentFileQuery) -> Result<Option<AttachmentFile>, VmError> {
    let store = store_handle()?;
    let store = store.lock().await;
    let attachments = AttachmentStore::from(&*store);
    let bytes = attachments.read_file(&q.commit, &q.path).map_err(|e| {
        VmError::panic(format!(
            "attachment commit {} path {}: {e}",
            q.commit, q.path
        ))
    })?;
    Ok(bytes.map(|bytes| AttachmentFile {
        path: q.path,
        bytes,
    }))
}

/// One file of an attachment, with its content.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct AttachmentFile {
    /// The path relative to the attachment's root
    #[rune(get)]
    pub path: String,
    #[rune(skip)]
    pub bytes: Vec<u8>,
}

impl AttachmentFile {
    #[rune::function(instance)]
    fn bytes(&self) -> Result<Bytes, VmError> {
        Ok(Bytes::from_slice(&self.bytes)?)
    }

    /// The content as UTF-8 text.
    #[rune::function(instance)]
    fn text(&self) -> Result<String, Error> {
        String::from_utf8(self.bytes.clone())
            .map_err(|e| Error::Decode(format!("{}: {e}", self.path)))
    }

    /// The content parsed as JSON.
    #[rune::function(instance)]
    fn json(&self) -> Result<Value, Error> {
        let parsed: serde_json::Value = serde_json::from_slice(&self.bytes)
            .map_err(|e| Error::Decode(format!("{}: {e}", self.path)))?;
        Ok(json_to_value(&parsed))
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AttachmentFile {{ path: {:?}, size: {} }}",
            self.path,
            self.bytes.len()
        )?;
        Ok(())
    }
}

/// A double-ended, exact-size iterator over a scan's attachments, in
/// dataset order.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Attachments {
    #[rune(skip)]
    items: Vec<Attachment>,
    #[rune(skip)]
    front: usize,
    #[rune(skip)]
    back: usize,
}

impl Attachments {
    fn new(items: Vec<Attachment>) -> Self {
        let back = items.len();
        Attachments {
            items,
            front: 0,
            back,
        }
    }

    #[rune::function(instance, keep, protocol = NEXT)]
    fn next(&mut self) -> Option<Attachment> {
        if self.front == self.back {
            return None;
        }
        let value = self.items.get(self.front)?.clone();
        self.front += 1;
        Some(value)
    }

    #[rune::function(instance, keep, protocol = NTH)]
    fn nth(&mut self, n: usize) -> Option<Attachment> {
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
    fn next_back(&mut self) -> Option<Attachment> {
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

    fn file(path: &str, bytes: &[u8]) -> AttachmentFile {
        AttachmentFile {
            path: path.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn file_decodes_text_and_json_and_reports_bad_content() {
        let mut vm = vm(r#"
            pub fn check(good, bad) {
                let n = good.json().unwrap().get("cleanupPeriodDays");
                let text = good.text().unwrap();
                let err = match bad.json() {
                    Ok(_) => "ok",
                    Err(gage::Error::Decode(_)) => "decode",
                    Err(_) => "other",
                };
                (n, text, good.bytes().len(), err, format!("{good:?}"))
            }
            "#);
        let good = file("settings.json", b"{\"cleanupPeriodDays\": 90}");
        let bad = file("x", b"{");
        let output = vm.call(["check"], (good, bad)).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (n, text, len, err, debug): (Option<i64>, String, i64, String, String) =
            rune::from_value(output).unwrap();
        assert_eq!(n, Some(90));
        assert_eq!(text, "{\"cleanupPeriodDays\": 90}");
        assert_eq!(len, 25);
        assert_eq!(err, "decode");
        assert_eq!(
            debug,
            "AttachmentFile { path: \"settings.json\", size: 25 }"
        );
    }

    #[test]
    fn attachments_iterate_in_order_with_the_full_iterator_surface() {
        let mut vm = vm(r#"
            pub fn check(attachments) {
                let n = attachments.len();
                let names = attachments.rev().map(|a| a.name.unwrap()).collect::<Vec>();
                (n, names)
            }
            "#);
        let attachment = |name: &str| Attachment {
            id: format!("id-{name}"),
            name: Some(name.to_string()),
            key: None,
            targets: rune::to_value(Vec::<String>::new()).unwrap(),
            root: "/r".to_string(),
            digest: None,
            commit: "c".to_string(),
        };
        let attachments = Attachments::new(vec![attachment("a"), attachment("b")]);
        let output = vm.call(["check"], (attachments,)).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (n, names): (i64, Vec<String>) = rune::from_value(output).unwrap();
        assert_eq!(n, 2);
        assert_eq!(names, ["b", "a"]);
    }

    /// `names` borrows its list, so the caller's list is still
    /// readable afterwards.
    #[test]
    fn attachments_names_leaves_the_caller_list_readable() {
        let mut vm = vm(r#"
            pub fn check(scan) {
                let names = ["a", "b"];
                let query = scan.attachments().names(names);
                (names.len(), names[1])
            }
            "#);
        let scan = Scan {
            id: "scan".into(),
            dataset: None,
        };
        let output = vm.call(["check"], (scan,)).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (len, second): (i64, String) = rune::from_value(output).unwrap();
        assert_eq!((len, second.as_str()), (2, "b"));
    }

    /// `names` on a filtered query borrows its list too, so both the
    /// first and the replacing list stay readable.
    #[test]
    fn filtered_attachments_names_leaves_the_caller_list_readable() {
        let mut vm = vm(r#"
            pub fn check(scan) {
                let first = ["a"];
                let second = ["b", "c"];
                let query = scan.attachments().names(first).names(second);
                (first[0], second.len())
            }
            "#);
        let scan = Scan {
            id: "scan".into(),
            dataset: None,
        };
        let output = vm.call(["check"], (scan,)).unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (first, len): (String, i64) = rune::from_value(output).unwrap();
        assert_eq!((first.as_str(), len), ("a", 2));
    }
}
