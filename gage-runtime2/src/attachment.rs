//! `scan().attachments()`: the attachments the scan's dataset links,
//! for scanners.
//!
//! `scan().attachments()` is an [`AttachmentsQuery`]; awaiting it
//! reads the dataset's `attachments.link` at the commit the scan
//! links and yields an [`Attachments`] iterator of [`Attachment`]
//! values in link order. `.name(name)` keeps the attachment with
//! that name. An attachment offers `files()`, awaited to the list of
//! file keys, and `file(key)`, awaited to `Some(AttachmentFile)` or
//! `None`. A file holds its bytes; `bytes()` and `text()` return
//! them, and `text()` and `json()` are fallible, returning
//! `Error::Decode` for content that is not UTF-8 or not JSON.

use gage_runtime::error::Error;
use gage_runtime::value::json_to_value;
use gage_store::{AttachmentStore, DatasetStore};
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Bytes, Formatter, Protocol, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::scan::{Scan, current};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.function_meta(Scan::attachments)?;
    m.ty::<AttachmentsQuery>()?;
    m.function_meta(AttachmentsQuery::name)?;
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
    /// The attachments the scan's dataset links, read when awaited.
    #[rune::function(instance)]
    fn attachments(&self) -> AttachmentsQuery {
        AttachmentsQuery { name: None }
    }
}

/// The value of `scan().attachments()`. Awaiting it runs the read.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentsQuery {
    #[rune(skip)]
    name: Option<String>,
}

impl AttachmentsQuery {
    /// Keep the attachment named `name`.
    #[rune::function(instance)]
    fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }
}

/// The dataset's attachments at the commit the scan links, in link
/// order. Without a dataset there are none.
async fn fetch_attachments(query: AttachmentsQuery) -> Result<Attachments, VmError> {
    let ctx = current()?;
    let Some(dataset) = &ctx.dataset else {
        return Ok(Attachments::new(Vec::new()));
    };
    let store = ctx.store.lock().await;
    let records = DatasetStore::from(&*store)
        .attachments_at(&dataset.commit_sha)
        .map_err(|e| VmError::panic(format!("attachments of dataset {}: {e}", dataset.id)))?;
    let items = records
        .into_iter()
        .filter(|r| query.name.as_ref().is_none_or(|name| *name == r.attrs.name))
        .map(|r| Attachment {
            id: r.id,
            name: r.attrs.name,
            commit: r.commit_sha,
        })
        .collect();
    Ok(Attachments::new(items))
}

/// An attachment, as a scanner sees it: its id and name and, held for
/// the runtime, the version the scan reads.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Attachment {
    /// The Gage object id
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub name: String,
    #[rune(skip)]
    pub commit: String,
}

impl Attachment {
    /// The attachment's file keys, read when awaited.
    #[rune::function(instance)]
    fn files(&self) -> AttachmentFilesQuery {
        AttachmentFilesQuery {
            commit: self.commit.clone(),
        }
    }

    /// The file at `key`, read when awaited: `Some(AttachmentFile)`,
    /// or `None` when the attachment has no such file.
    #[rune::function(instance)]
    fn file(&self, key: &str) -> AttachmentFileQuery {
        AttachmentFileQuery {
            commit: self.commit.clone(),
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
    commit: String,
}

async fn fetch_files(q: AttachmentFilesQuery) -> Result<Vec<String>, VmError> {
    let ctx = current()?;
    let store = ctx.store.lock().await;
    let files = AttachmentStore::from(&*store)
        .files(&q.commit)
        .map_err(|e| VmError::panic(format!("attachment files: {e}")))?;
    Ok(files.into_iter().map(|f| f.key).collect())
}

/// The value of `attachment.file(key)`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct AttachmentFileQuery {
    #[rune(skip)]
    commit: String,
    #[rune(skip)]
    key: String,
}

async fn fetch_file(q: AttachmentFileQuery) -> Result<Option<AttachmentFile>, VmError> {
    let ctx = current()?;
    let store = ctx.store.lock().await;
    let bytes = AttachmentStore::from(&*store)
        .read_file(&q.commit, &q.key)
        .map_err(|e| VmError::panic(format!("attachment file {}: {e}", q.key)))?;
    Ok(bytes.map(|bytes| AttachmentFile { key: q.key, bytes }))
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
/// link order.
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
            commit: format!("commit-{name}"),
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
