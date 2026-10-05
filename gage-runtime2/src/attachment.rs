//! `scan().attachments()` and `scan().attachment(name)`: the
//! attachments the scan's dataset holds, for scanners.
//!
//! `scan().attachments()` is an [`AttachmentsQuery`]; awaiting it
//! reads the scan-scoped `attachment` table and yields an
//! [`Attachments`] iterator of [`Attachment`] values in dataset
//! order. `scan().attachment(name)` is an
//! [`AttachmentQuery`]; awaiting it yields `Some(Attachment)` or
//! `None`. A name names at most one attachment, since the object id
//! derives from it. An attachment offers `files()`, awaited to the
//! list of file keys, and `file(key)`, awaited to
//! `Some(AttachmentFile)` or `None`, both read from the
//! `attachment_file` table. A file holds its bytes;
//! `bytes()` and `text()` return them, and `text()` and `json()` are
//! fallible, returning `Error::Decode` for content that is not UTF-8
//! or not JSON.

use datafusion::arrow::array::BinaryArray;
use gage_runtime::error::Error;
use gage_runtime::value::json_to_value;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Bytes, Formatter, Protocol, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::scan::{Scan, current, run, sql_str, string_column};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.function_meta(Scan::attachments)?;
    m.function_meta(Scan::attachment)?;
    m.ty::<AttachmentsQuery>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |_q: AttachmentsQuery| async move {
        Ok::<_, VmError>(Attachments::new(fetch_attachments().await?))
    })?;
    m.ty::<AttachmentQuery>()?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: AttachmentQuery| async move {
        fetch_attachment(q).await
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
        AttachmentsQuery {}
    }

    /// The attachment named `name` in the scan's dataset, read when
    /// awaited: `Some(Attachment)` or `None`.
    #[rune::function(instance)]
    fn attachment(&self, name: &str) -> AttachmentQuery {
        AttachmentQuery {
            name: name.to_string(),
        }
    }
}

/// The value of `scan().attachments()`. Awaiting it runs the read.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentsQuery {}

/// The value of `scan().attachment(name)`. Awaiting it runs the read.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentQuery {
    #[rune(skip)]
    name: String,
}

async fn fetch_attachment(q: AttachmentQuery) -> Result<Option<Attachment>, VmError> {
    let sql = format!(
        "SELECT id, name FROM attachment WHERE name = '{}'",
        sql_str(&q.name)
    );
    Ok(attachments(&sql).await?.into_iter().next())
}

/// The dataset's attachments at the commits the scan links, in
/// dataset order. Without a dataset there are none.
async fn fetch_attachments() -> Result<Vec<Attachment>, VmError> {
    attachments("SELECT id, name FROM attachment").await
}

async fn attachments(sql: &str) -> Result<Vec<Attachment>, VmError> {
    let ctx = current()?;
    let batches = run(ctx.scan_context().await?, sql).await?;
    let mut out = Vec::new();
    for batch in &batches {
        let ids = string_column(batch, 0);
        let names = string_column(batch, 1);
        for i in 0..batch.num_rows() {
            out.push(Attachment {
                id: ids.value(i).to_string(),
                name: names.value(i).to_string(),
            });
        }
    }
    Ok(out)
}

/// An attachment, as a scanner sees it: its id and name.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Attachment {
    /// The Gage object id
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: String,
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
            "Attachment {{ id: {:?}, name: {:?} }}",
            self.id, self.name
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
    let ctx = current()?;
    let sql = format!(
        "SELECT key FROM attachment_file WHERE attachment_id = '{}' ORDER BY key",
        sql_str(&q.attachment_id)
    );
    let batches = run(ctx.scan_context().await?, &sql).await?;
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
    let ctx = current()?;
    let sql = format!(
        "SELECT content FROM attachment_file WHERE attachment_id = '{}' AND key = '{}'",
        sql_str(&q.attachment_id),
        sql_str(&q.key)
    );
    let batches = run(ctx.scan_context().await?, &sql).await?;
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
                let names = attachments.rev().map(|a| a.name).collect::<Vec>();
                (n, names)
            }
            "#);
        let attachment = |name: &str| Attachment {
            id: format!("id-{name}"),
            name: name.to_string(),
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
