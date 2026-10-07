//! `scan().attachments()` and `session.attachments()`: the
//! attachments the scan's dataset holds, for scanners.
//!
//! `scan().attachments()` is an [`AttachmentsQuery`]. `.name(pattern)`
//! keeps the attachments whose name matches a `*`-glob, as a task's
//! `wants` does; `.target(object)` keeps those with the object among
//! their targets, given as a `Session`, an id or prefix, or a Gage
//! URL. `session.attachments()` is the query with the session as
//! target.
//! Awaiting the query reads the scoped `attachment` table and yields
//! an [`Attachments`] iterator of [`Attachment`] values in dataset
//! order. A name is a selector, not an identifier, so there is no
//! lookup of one attachment by name.
//!
//! An attachment offers `files()`, awaited to the list of file keys,
//! and `file(key)`, awaited to `Some(AttachmentFile)` or `None`, both
//! read from the `attachment_file` table. A file holds its bytes;
//! `bytes()` and `text()` return them, and `text()` and `json()` are
//! fallible, returning `Error::Decode` for content that is not UTF-8
//! or not JSON.

use datafusion::arrow::array::{Array, BinaryArray, ListArray, StringArray};
use gage_runtime::error::Error;
use gage_runtime::value::json_to_value;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Bytes, Formatter, Protocol, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::note::name_predicate;
use crate::scan::{Scan, Session, dataset_query, run, sql_str, string_column, target_url};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.function_meta(Scan::attachments)?;
    m.function_meta(Session::attachments)?;
    m.ty::<AttachmentsQuery>()?;
    m.function_meta(AttachmentsQuery::name)?;
    m.function_meta(AttachmentsQuery::target)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: AttachmentsQuery| async move {
        fetch_attachments(q).await
    })?;
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
        AttachmentsQuery {
            name: None,
            target: None,
        }
    }
}

impl Session {
    /// The attachments about this session, read when awaited.
    #[rune::function(instance)]
    fn attachments(&self) -> AttachmentsQuery {
        AttachmentsQuery {
            name: None,
            target: Some(Target::Url(format!("session:{}", self.id))),
        }
    }
}

/// A target filter, resolved to a URL when the query runs.
#[derive(Debug, Clone)]
enum Target {
    Url(String),
    Value(Value),
}

/// The value of `scan().attachments()`. Awaiting it runs the read.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentsQuery {
    #[rune(skip)]
    name: Option<String>,
    #[rune(skip)]
    target: Option<Target>,
}

impl AttachmentsQuery {
    /// Keep the attachments whose name matches `pattern`: `*` matches
    /// any run of characters; a pattern without `*` is an exact name.
    #[rune::function(instance)]
    fn name(mut self, pattern: &str) -> Self {
        self.name = Some(pattern.to_string());
        self
    }

    /// Keep the attachments with `object` among their targets: a
    /// `Session`, an object id or prefix, or a Gage URL.
    #[rune::function(instance)]
    fn target(mut self, object: Value) -> Self {
        self.target = Some(Target::Value(object));
        self
    }
}

/// The dataset's attachments matching the query, in dataset order.
/// Without a dataset there are none. The inner error is a target
/// argument that names nothing.
async fn fetch_attachments(q: AttachmentsQuery) -> Result<Result<Attachments, Error>, VmError> {
    let mut clauses = Vec::new();
    if let Some(pattern) = &q.name {
        clauses.push(format!(
            "({})",
            name_predicate("name", std::slice::from_ref(pattern))
        ));
    }
    if let Some(target) = &q.target {
        let url = match target {
            Target::Url(url) => url.clone(),
            Target::Value(value) => match target_url(value).await? {
                Ok(url) => url,
                Err(e) => return Ok(Err(e)),
            },
        };
        clauses.push(format!("array_has(targets, '{}')", sql_str(&url)));
    }
    let sql = if clauses.is_empty() {
        "SELECT id, name, key, targets, root FROM attachment".to_string()
    } else {
        format!(
            "SELECT id, name, key, targets, root FROM attachment WHERE {}",
            clauses.join(" AND ")
        )
    };
    Ok(Ok(Attachments::new(attachments(&sql).await?)))
}

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
}

impl Attachment {
    /// The attachment's file keys, read when awaited.
    #[rune::function(instance)]
    fn files(&self) -> AttachmentFilesQuery {
        AttachmentFilesQuery {
            attachment_id: self.id.clone(),
        }
    }

    /// The file at `key`, read when awaited: `Some(AttachmentFile)`,
    /// or `None` when the attachment has no such file.
    #[rune::function(instance)]
    fn file(&self, key: &str) -> AttachmentFileQuery {
        AttachmentFileQuery {
            attachment_id: self.id.clone(),
            key: key.to_string(),
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
    attachment_id: String,
}

async fn fetch_files(q: AttachmentFilesQuery) -> Result<Vec<String>, VmError> {
    let sql = format!(
        "SELECT key FROM attachment_file WHERE attachment_id = '{}' ORDER BY key",
        sql_str(&q.attachment_id)
    );
    let batches = run(&dataset_query().await?, &sql).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let keys = string_column(batch, 0);
        for i in 0..batch.num_rows() {
            out.push(keys.value(i).to_string());
        }
    }
    Ok(out)
}

/// The value of `attachment.file(key)`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentFileQuery {
    #[rune(skip)]
    attachment_id: String,
    #[rune(skip)]
    key: String,
}

async fn fetch_file(q: AttachmentFileQuery) -> Result<Option<AttachmentFile>, VmError> {
    let sql = format!(
        "SELECT content FROM attachment_file WHERE attachment_id = '{}' AND key = '{}'",
        sql_str(&q.attachment_id),
        sql_str(&q.key)
    );
    let batches = run(&dataset_query().await?, &sql).await?;
    let Some(batch) = batches.iter().find(|b| b.num_rows() > 0) else {
        return Ok(None);
    };
    let contents = batch
        .column(0)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("attachment_file content is a binary column");
    Ok(Some(AttachmentFile {
        key: q.key,
        bytes: contents.value(0).to_vec(),
    }))
}

/// One file of an attachment, with its content.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct AttachmentFile {
    /// The path relative to the attachment's root
    #[rune(get)]
    pub key: String,
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
            .map_err(|e| Error::Decode(format!("{}: {e}", self.key)))
    }

    /// The content parsed as JSON.
    #[rune::function(instance)]
    fn json(&self) -> Result<Value, Error> {
        let parsed: serde_json::Value = serde_json::from_slice(&self.bytes)
            .map_err(|e| Error::Decode(format!("{}: {e}", self.key)))?;
        Ok(json_to_value(&parsed))
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(
            f,
            "AttachmentFile {{ key: {:?}, size: {} }}",
            self.key,
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

    fn file(key: &str, bytes: &[u8]) -> AttachmentFile {
        AttachmentFile {
            key: key.to_string(),
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
        assert_eq!(debug, "AttachmentFile { key: \"settings.json\", size: 25 }");
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
}
