//! Pending session publish operations.
//!
//! A publish is staged as a directory under Gage home,
//! `publish/<session id>/`, named by the new session's object id.
//! The directory holds the derived session's final tree layout:
//! `attrs.json` with `origin`, the `files.d` content, and
//! `filtered.json`. It is the same shape an added session's tree has,
//! so one decoder serves both the staged directory and a committed
//! session, the way a scan directory mirrors a stored scan. Beside it,
//! `publish/<session id>.publish.json` is the publish definition: the
//! instructions the staged session is derived from, which may hold
//! anything and is never committed. The session enters the store only
//! on apply, which commits the staged directory as it stands and then
//! deletes it with the definition; abort deletes both and writes
//! nothing.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use gage_core::uuid::new_uuid;
use serde_json::{Value, json};

use crate::index::IdMatch;
use crate::object::{ObjectTree, object_ref};
use crate::scan::import_opaque_tree;
use crate::session::{FILES_TREE, OBJECT_TYPE, OBJECT_VERSION, SessionAttrsRecord};
use crate::{SessionRecord, SessionStore, Store, StoreError};

const PUBLISH_DIR: &str = "publish";
const ATTRS_FILE: &str = "attrs.json";
pub(crate) const FILTERED_FILE: &str = "filtered.json";
const DEFINITION_SUFFIX: &str = ".publish.json";

/// Whether the data layer includes pending publish working trees in
/// the session tables, set by `GAGE_SHOW_PUBLISHING=1`. Only the
/// exact value `1` turns it on.
pub fn show_publishing() -> bool {
    std::env::var_os("GAGE_SHOW_PUBLISHING").as_deref() == Some(OsStr::new("1"))
}

/// One pending publish working tree, read from its staging directory.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPublish {
    /// The new session's object id, the directory name.
    pub id: String,
    pub attrs: SessionAttrsRecord,
    pub root: PathBuf,
    /// UNIX time millis of the working tree's last change: the newest
    /// mtime of the directory, `attrs.json`, and `filtered.json`. A
    /// rebuild rewrites the two files in place, which leaves the
    /// directory's own mtime alone, so the files are what move it.
    pub modified_ms: i64,
}

/// The parent of publish staging directories, `publish/` under Gage
/// home.
pub fn publish_root() -> PathBuf {
    gage_core::config::gage_home().join(PUBLISH_DIR)
}

/// The on-disk layout of one pending publish, `publish/<id>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDirLayout {
    root: PathBuf,
}

impl PublishDirLayout {
    /// The layout rooted at `root`, `publish/<id>/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The new session's id: the directory name.
    pub fn id(&self) -> Option<&str> {
        self.root.file_name().and_then(|n| n.to_str())
    }

    pub fn attrs_file(&self) -> PathBuf {
        self.root.join(ATTRS_FILE)
    }

    /// The opaque content subtree, `files.d/`.
    pub fn files_dir(&self) -> PathBuf {
        self.root.join(FILES_TREE)
    }

    pub fn filtered_file(&self) -> PathBuf {
        self.root.join(FILTERED_FILE)
    }

    /// The records of the staged `filtered.json`.
    pub fn read_filtered(&self) -> Result<Vec<FilteredRecord>, StoreError> {
        let bytes = fs::read(self.filtered_file()).map_err(StoreError::ReadContent)?;
        parse_filtered(&bytes)
    }
}

/// One rule of a session's `filtered.json`: what its publish applied
/// to the original. Every line number is the original's; the derived
/// session's lines are renumbered around the dropped ones. Span
/// offsets are positions in the published text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredRecord {
    pub kind: FilteredKind,
    /// The original line, 1-based: the first dropped line of a `Line`
    /// record, the line of a `Text` record. A `Value` or `Attr` record
    /// is on no line and carries 0.
    pub line: u32,
    /// The last dropped line of a `Line` record, inclusive.
    pub line_end: Option<u32>,
    /// What the record refers to: nothing for a `Line`, the JSONPath
    /// selector of a `Value`, the JSON Pointer to the text value of a
    /// `Text`, the attribute name of an `Attr`.
    pub reference: Option<String>,
    /// Zero-based UTF-8 byte offsets of a `Text` record's span in the
    /// published value, `span_end` exclusive. A removal is a span of
    /// zero length.
    pub span_start: Option<u64>,
    pub span_end: Option<u64>,
    pub reason: String,
    pub author: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilteredKind {
    /// One or more lines were left out of the derived session.
    Line,
    /// Every value a JSONPath selects was left out of its entry.
    Value,
    /// A byte span of one text value was removed or replaced by the
    /// marker.
    Text,
    /// An attribute of `attrs.json` was left out.
    Attr,
}

impl FilteredKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FilteredKind::Line => "line",
            FilteredKind::Value => "value",
            FilteredKind::Text => "text",
            FilteredKind::Attr => "attr",
        }
    }
}

/// The lists of `filtered.json` in file order, each holding the
/// records of one kind.
const FILTERED_LISTS: [(&str, FilteredKind); 4] = [
    ("lines", FilteredKind::Line),
    ("values", FilteredKind::Value),
    ("text", FilteredKind::Text),
    ("attrs", FilteredKind::Attr),
];

/// Decode the `filtered.json` text: an object whose `lines`, `values`,
/// `text`, and `attrs` lists hold the records of each kind as
/// `[line or [line, line_end], reason, author]`,
/// `[path, reason, author]`,
/// `[line, pointer, span_start, span_end, reason, author]`, and
/// `[name, reason, author]`. A missing list is empty, and a key this
/// build does not know is passed over. Records come back list by list
/// in file order.
pub fn parse_filtered(bytes: &[u8]) -> Result<Vec<FilteredRecord>, StoreError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| StoreError::Parse(format!("{FILTERED_FILE}: {e}")))?;
    let Some(object) = value.as_object() else {
        return Err(StoreError::Parse(format!(
            "{FILTERED_FILE}: expected an object of record lists"
        )));
    };
    let mut out = Vec::new();
    for (key, kind) in FILTERED_LISTS {
        let Some(list) = object.get(key) else {
            continue;
        };
        let Some(records) = list.as_array() else {
            return Err(StoreError::Parse(format!(
                "{FILTERED_FILE}: {key} is not an array"
            )));
        };
        for (i, record) in records.iter().enumerate() {
            out.push(parse_filtered_record(kind, record).map_err(|e| {
                StoreError::Parse(format!("{FILTERED_FILE} {key} record {i}: {e}"))
            })?);
        }
    }
    Ok(out)
}

fn parse_filtered_record(kind: FilteredKind, record: &Value) -> Result<FilteredRecord, String> {
    let fields = record
        .as_array()
        .ok_or_else(|| "expected an array".to_string())?;
    let field = |i: usize| fields.get(i).ok_or_else(|| format!("missing field {i}"));
    let text = |i: usize| {
        field(i)?
            .as_str()
            .map(String::from)
            .ok_or_else(|| format!("field {i} is not a string"))
    };
    let line_of = |v: &Value, what: &str| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("{what} is not a line number"))
    };
    let offset = |i: usize| {
        field(i)?
            .as_u64()
            .ok_or_else(|| format!("field {i} is not a byte offset"))
    };
    let expect = |n: usize| {
        if fields.len() == n {
            Ok(())
        } else {
            Err(format!("expected {n} fields, found {}", fields.len()))
        }
    };
    let record = match kind {
        FilteredKind::Line => {
            expect(3)?;
            let (line, line_end) = match field(0)? {
                Value::Array(pair) => match pair.as_slice() {
                    [start, end] => {
                        let line = line_of(start, "range start")?;
                        let line_end = line_of(end, "range end")?;
                        if line_end < line {
                            return Err("range end is before its start".to_string());
                        }
                        (line, line_end)
                    }
                    _ => return Err("field 0 is not a line or a range".to_string()),
                },
                single => {
                    let line = line_of(single, "field 0")?;
                    (line, line)
                }
            };
            FilteredRecord {
                kind,
                line,
                line_end: Some(line_end),
                reference: None,
                span_start: None,
                span_end: None,
                reason: text(1)?,
                author: text(2)?,
            }
        }
        FilteredKind::Value | FilteredKind::Attr => {
            expect(3)?;
            FilteredRecord {
                kind,
                line: 0,
                line_end: None,
                reference: Some(text(0)?),
                span_start: None,
                span_end: None,
                reason: text(1)?,
                author: text(2)?,
            }
        }
        FilteredKind::Text => {
            expect(6)?;
            let (span_start, span_end) = (offset(2)?, offset(3)?);
            if span_end < span_start {
                return Err("span end is before its start".to_string());
            }
            FilteredRecord {
                kind,
                line: line_of(field(0)?, "field 0")?,
                line_end: None,
                reference: Some(text(1)?),
                span_start: Some(span_start),
                span_end: Some(span_end),
                reason: text(4)?,
                author: text(5)?,
            }
        }
    };
    Ok(record)
}

/// Encode `records` as the `filtered.json` text: each list in file
/// order with one record per line, in the order the records are
/// given, so the file reads as the design shows it.
pub fn format_filtered(records: &[FilteredRecord]) -> String {
    let mut out = String::from("{\n");
    for (n, (key, kind)) in FILTERED_LISTS.iter().enumerate() {
        let of_kind: Vec<Value> = records
            .iter()
            .filter(|r| r.kind == *kind)
            .map(filtered_value)
            .collect();
        out.push_str(&format!("  \"{key}\": ["));
        for (i, value) in of_kind.iter().enumerate() {
            out.push_str("\n    ");
            out.push_str(&value.to_string());
            if i + 1 < of_kind.len() {
                out.push(',');
            } else {
                out.push_str("\n  ");
            }
        }
        out.push(']');
        out.push_str(if n + 1 < FILTERED_LISTS.len() {
            ",\n"
        } else {
            "\n"
        });
    }
    out.push_str("}\n");
    out
}

/// One record as its list holds it.
fn filtered_value(r: &FilteredRecord) -> Value {
    match r.kind {
        FilteredKind::Line => {
            let end = r.line_end.unwrap_or(r.line);
            let lines = if end == r.line {
                json!(r.line)
            } else {
                json!([r.line, end])
            };
            json!([lines, r.reason, r.author])
        }
        FilteredKind::Value | FilteredKind::Attr => json!([r.reference, r.reason, r.author]),
        FilteredKind::Text => json!([
            r.line,
            r.reference,
            r.span_start,
            r.span_end,
            r.reason,
            r.author
        ]),
    }
}

/// The new session that `init` staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishInit {
    /// The new session's object id, the directory name and the handle
    /// for every later publish command.
    pub id: String,
    /// The `origin` the derived session records, `session:<id>@<commit>`.
    pub origin: String,
}

/// Publish operations over an opened store.
pub struct PublishStore<'a> {
    store: &'a Store,
    root: PathBuf,
}

impl<'a> From<&'a Store> for PublishStore<'a> {
    fn from(store: &'a Store) -> Self {
        PublishStore {
            store,
            root: publish_root(),
        }
    }
}

impl<'a> PublishStore<'a> {
    /// Stage the publish under `root` instead of Gage home. For tests.
    pub fn with_root(store: &'a Store, root: impl Into<PathBuf>) -> Self {
        PublishStore {
            store,
            root: root.into(),
        }
    }

    /// The definition file of the pending publish `id`,
    /// `publish/<id>.publish.json`: the instructions the staged session
    /// is derived from. Never committed.
    pub fn definition_file(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}{DEFINITION_SUFFIX}"))
    }

    /// The staging directory of the pending publish `id`.
    pub fn layout(&self, id: &str) -> PublishDirLayout {
        PublishDirLayout::new(self.root.join(id))
    }

    /// Start a publish of `original`, a stored session. Generates the
    /// new session id, creates the staging directory, copies
    /// `original`'s content into `files.d`, writes `attrs.json` with
    /// `origin` naming `original` and an empty `filtered.json`.
    /// Returns the new id. The definition beside the directory is the
    /// publish engine's to write. No store object is written.
    pub fn init(&self, original: &SessionRecord) -> Result<PublishInit, StoreError> {
        let id = new_uuid();
        let origin = format!("session:{}@{}", original.id, original.commit_sha);
        let layout = PublishDirLayout::new(self.root.join(&id));
        fs::create_dir_all(layout.files_dir()).map_err(StoreError::ReadContent)?;

        let content = SessionStore::from(self.store).content(&original.commit_sha);
        let mut size: u64 = 0;
        for path in content.paths().map_err(StoreError::ReadContent)? {
            let mut reader = content.open(&path).map_err(StoreError::ReadContent)?;
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(StoreError::ReadContent)?;
            size += bytes.len() as u64;
            let dest = layout.files_dir().join(&path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(StoreError::ReadContent)?;
            }
            fs::write(&dest, &bytes).map_err(StoreError::ReadContent)?;
        }

        // A derived session has no native artifact, so the fingerprint
        // fields are zero; nothing re-adds a derived session through
        // the path that reads them.
        let attrs = SessionAttrsRecord {
            driver: original.attrs.driver.clone(),
            origin: origin.clone(),
            native_mtime: 0,
            native_size: 0,
            session_type: original.attrs.session_type.clone(),
            content_format: original.attrs.content_format.clone(),
            size,
            project: original.attrs.project.clone(),
            summary: original.attrs.summary.clone(),
        };
        let value = serde_json::to_value(&attrs)
            .map_err(|e| StoreError::Parse(format!("publish attrs encode: {e}")))?;
        let mut attrs_bytes =
            serde_json::to_vec(&value).map_err(|e| StoreError::Parse(format!("{e}")))?;
        attrs_bytes.push(b'\n');
        fs::write(layout.attrs_file(), &attrs_bytes).map_err(StoreError::ReadContent)?;
        fs::write(layout.filtered_file(), format_filtered(&[])).map_err(StoreError::ReadContent)?;

        Ok(PublishInit { id, origin })
    }

    /// The one pending publish whose id is `id_or_prefix` or starts
    /// with it. No match is `ObjectNotFound`; more than one is
    /// `AmbiguousId` naming the candidates.
    pub fn get(&self, id_or_prefix: &str) -> Result<PendingPublish, StoreError> {
        let mut matches: Vec<PendingPublish> = self
            .list()?
            .into_iter()
            .filter(|p| p.id.starts_with(id_or_prefix))
            .collect();
        match matches.len() {
            0 => Err(StoreError::ObjectNotFound(id_or_prefix.to_string())),
            1 => Ok(matches.remove(0)),
            _ => Err(StoreError::AmbiguousId(
                id_or_prefix.to_string(),
                matches
                    .into_iter()
                    .map(|p| IdMatch {
                        id: p.id,
                        tip_sha: String::new(),
                        object_type: OBJECT_TYPE.to_string(),
                        deleted: false,
                    })
                    .collect(),
            )),
        }
    }

    /// Commit the staged directory of the pending publish `id` as a
    /// new session object, as it stands: `attrs.json` as the attrs,
    /// `filtered.json` as a root blob, `files.d` as the content
    /// subtree. The object's ref must not exist. The staging
    /// directory and the definition are deleted afterward. Returns the
    /// commit SHA. Nothing in the staged content is checked here; the
    /// publish engine validates before it calls.
    pub fn apply(&self, id: &str) -> Result<String, StoreError> {
        let layout = self.layout(id);
        if !layout.root().is_dir() {
            return Err(StoreError::ObjectNotFound(id.to_string()));
        }
        if self.store.rev_parse(&object_ref(id))?.is_some() {
            return Err(StoreError::ObjectExists(id.to_string()));
        }
        let read = |path: &Path| fs::read(path).map_err(StoreError::ReadContent);
        let attrs: serde_json::Value = serde_json::from_slice(&read(&layout.attrs_file())?)
            .map_err(|e| StoreError::Parse(format!("publish attrs {id}: {e}")))?;
        let origin = attrs
            .get("origin")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| StoreError::Parse(format!("publish attrs {id}: no origin")))?
            .to_string();
        let filtered = read(&layout.filtered_file())?;
        let files_tree =
            import_opaque_tree(self.store.path(), &layout.files_dir())?.ok_or_else(|| {
                StoreError::Parse(format!(
                    "publish {id}: no {} directory",
                    layout.files_dir().display()
                ))
            })?;

        let mut tree = ObjectTree {
            attrs: Some(attrs),
            ..ObjectTree::default()
        };
        tree.blobs.insert(FILTERED_FILE.to_string(), filtered);
        tree.subtrees.insert(FILES_TREE.to_string(), files_tree);
        let message = format!("session publish: {origin}");
        let commit_sha = self
            .store
            .create(OBJECT_TYPE, OBJECT_VERSION, id, &tree, &message)?;
        self.remove_staging(id)?;
        Ok(commit_sha)
    }

    /// Delete the staging directory and the definition of the pending
    /// publish `id`. Nothing was written to the store, so nothing in
    /// the store changes. An unknown id is `ObjectNotFound`; a
    /// definition already deleted by hand is not an error.
    pub fn abort(&self, id: &str) -> Result<(), StoreError> {
        self.remove_staging(id)
    }

    fn remove_staging(&self, id: &str) -> Result<(), StoreError> {
        let root = self.root.join(id);
        match fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::ObjectNotFound(id.to_string()));
            }
            Err(e) => return Err(StoreError::ReadContent(e)),
        }
        match fs::remove_file(self.definition_file(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::ReadContent(e)),
        }
    }

    /// Every pending publish working tree under the publish root, in
    /// id order. A directory without a readable `attrs.json` is
    /// skipped, since it is mid-write or not a publish. Empty when the
    /// root does not exist.
    pub fn list(&self) -> Result<Vec<PendingPublish>, StoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::ReadContent(e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let dir = entry.map_err(StoreError::ReadContent)?.path();
            if !dir.is_dir() {
                continue;
            }
            let layout = PublishDirLayout::new(&dir);
            let bytes = match fs::read(layout.attrs_file()) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StoreError::ReadContent(e)),
            };
            let attrs: SessionAttrsRecord = serde_json::from_slice(&bytes)
                .map_err(|e| StoreError::Parse(format!("publish attrs {}: {e}", dir.display())))?;
            let Some(id) = layout.id().map(String::from) else {
                continue;
            };
            let modified_ms = [dir.clone(), layout.attrs_file(), layout.filtered_file()]
                .iter()
                .map(|p| mtime_ms(p))
                .max()
                .unwrap_or(0);
            out.push(PendingPublish {
                id,
                attrs,
                root: dir,
                modified_ms,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

/// The mtime of `path` in UNIX millis; 0 when it cannot be read.
fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_store;

    /// A rebuild rewrites `filtered.json` in place without touching the
    /// directory, so the listing's `modified_ms` follows the file.
    #[test]
    fn modified_follows_the_newest_file_in_the_working_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish = PublishStore::with_root(&store, tmp.path().join("publish"));
        let init = publish.init(&original).unwrap();
        let before = publish.list().unwrap()[0].modified_ms;

        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(90);
        let filtered = fs::File::options()
            .write(true)
            .open(publish.layout(&init.id).filtered_file())
            .unwrap();
        filtered.set_modified(later).unwrap();

        let after = publish.list().unwrap()[0].modified_ms;
        assert!(after > before, "{before} -> {after}");
        assert_eq!(
            after,
            later.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
        );
    }

    #[test]
    fn init_stages_a_derived_session_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "hello\nworld\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();

        let publish_dir = tmp.path().join("publish");
        let init = PublishStore::with_root(&store, &publish_dir)
            .init(&original)
            .unwrap();

        let layout = PublishDirLayout::new(publish_dir.join(&init.id));
        assert_eq!(layout.id(), Some(init.id.as_str()));
        assert_eq!(
            init.origin,
            format!("session:{}@{}", original.id, original.commit_sha)
        );

        let attrs: SessionAttrsRecord =
            serde_json::from_slice(&fs::read(layout.attrs_file()).unwrap()).unwrap();
        assert_eq!(attrs.origin, init.origin);
        assert_eq!(attrs.driver, original.attrs.driver);
        assert_eq!(attrs.content_format, original.attrs.content_format);
        assert_eq!(attrs.size, 12);
        assert_eq!(attrs.native_mtime, 0);
        assert_eq!(attrs.native_size, 0);

        assert_eq!(
            fs::read_to_string(layout.filtered_file()).unwrap(),
            "{\n  \"lines\": [],\n  \"values\": [],\n  \"text\": [],\n  \"attrs\": []\n}\n"
        );
        let staged = fs::read_to_string(layout.files_dir().join("session.jsonl")).unwrap();
        assert_eq!(staged, "hello\nworld\n");
    }

    #[test]
    fn list_returns_pending_working_trees_in_id_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();

        let publish_dir = tmp.path().join("publish");
        let publish = PublishStore::with_root(&store, &publish_dir);
        assert!(publish.list().unwrap().is_empty());

        let a = publish.init(&original).unwrap();
        let b = publish.init(&original).unwrap();

        let listed = publish.list().unwrap();
        assert_eq!(listed.len(), 2);
        let mut ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        let mut expected = vec![a.id.as_str(), b.id.as_str()];
        ids.sort_unstable();
        expected.sort_unstable();
        assert_eq!(ids, expected);
        assert!(listed.iter().all(|p| p.attrs.origin == a.origin));
    }

    /// `get` resolves a full id or a unique prefix, reports no match,
    /// and reports an ambiguous prefix with its candidates.
    #[test]
    fn get_resolves_a_pending_publish_by_id_or_unique_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish = PublishStore::with_root(&store, tmp.path().join("publish"));
        let a = publish.init(&original).unwrap();
        let b = publish.init(&original).unwrap();

        assert_eq!(publish.get(&a.id).unwrap().id, a.id);
        // Ids are random, so find a prefix of `a` that `b` does not share
        let unique =
            a.id.char_indices()
                .map(|(i, _)| &a.id[..=i])
                .find(|p| !b.id.starts_with(p))
                .unwrap();
        assert_eq!(publish.get(unique).unwrap().id, a.id);

        let missing = publish.get("zzzz").unwrap_err();
        assert!(matches!(missing, StoreError::ObjectNotFound(ref p) if p == "zzzz"));

        let ambiguous = publish.get("").unwrap_err();
        assert!(
            matches!(ambiguous, StoreError::AmbiguousId(_, ref c) if c.len() == 2),
            "{ambiguous}"
        );
    }

    /// `apply` commits the staged directory as it stands, with
    /// `filtered.json` as a root blob beside the session's usual
    /// entries, and deletes the staging directory and the definition.
    /// A second apply is `ObjectNotFound`.
    #[test]
    fn apply_commits_the_staged_directory_and_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "hello\nworld\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish_dir = tmp.path().join("publish");
        let publish = PublishStore::with_root(&store, &publish_dir);
        let init = publish.init(&original).unwrap();
        let layout = publish.layout(&init.id);
        fs::write(layout.files_dir().join("session.jsonl"), "hello\n").unwrap();
        let filtered = "{\n  \"lines\": [\n    [2,\"scope\",\"user:test\"]\n  ]\n}\n";
        fs::write(layout.filtered_file(), filtered).unwrap();
        fs::write(publish.definition_file(&init.id), b"{}\n").unwrap();
        let staged_attrs = fs::read_to_string(layout.attrs_file()).unwrap();

        let commit = publish.apply(&init.id).unwrap();

        let ref_path = crate::object::object_ref(&init.id);
        let listing = crate::git::run(crate::git::git_in(
            store.path(),
            ["ls-tree", "--name-only", &ref_path],
        ))
        .unwrap();
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            vec![
                "attrs.json",
                "created",
                "files.d",
                "filtered.json",
                "id",
                "modified",
                "type"
            ]
        );
        let cat = |path: &str| {
            crate::git::run(crate::git::git_in(
                store.path(),
                ["cat-file", "-p", &format!("{ref_path}:{path}")],
            ))
            .unwrap()
        };
        assert_eq!(cat("type"), "gage::session 1\n");
        assert_eq!(cat("attrs.json"), staged_attrs);
        assert_eq!(cat("filtered.json"), filtered);
        assert_eq!(cat("files.d/session.jsonl"), "hello\n");

        let published = SessionStore::from(&store).at_commit(&commit).unwrap();
        assert_eq!(published.id, init.id);
        assert_eq!(published.attrs.origin, init.origin);
        assert_eq!(SessionStore::from(&store).iter().unwrap().count(), 2);
        assert!(!publish_dir.join(&init.id).exists());
        assert!(!publish.definition_file(&init.id).exists());
        assert!(publish.list().unwrap().is_empty());

        let again = publish.apply(&init.id).unwrap_err();
        assert!(matches!(again, StoreError::ObjectNotFound(ref id) if *id == init.id));
    }

    /// `apply` refuses when the object's ref already exists and leaves
    /// the staging directory in place.
    #[test]
    fn apply_refuses_an_existing_object() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish_dir = tmp.path().join("publish");
        let publish = PublishStore::with_root(&store, &publish_dir);
        let init = publish.init(&original).unwrap();
        // Stage a second publish under the id of the first, as a copy
        // of its directory, after the first is applied
        let twin = publish_dir.join("twin");
        copy_dir(&publish_dir.join(&init.id), &twin);
        publish.apply(&init.id).unwrap();
        fs::rename(&twin, publish_dir.join(&init.id)).unwrap();

        let exists = publish.apply(&init.id).unwrap_err();
        assert!(matches!(exists, StoreError::ObjectExists(ref id) if *id == init.id));
        assert!(publish_dir.join(&init.id).is_dir());
        assert_eq!(SessionStore::from(&store).iter().unwrap().count(), 2);
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let dest = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &dest);
            } else {
                fs::copy(entry.path(), dest).unwrap();
            }
        }
    }

    /// The record parser reads every list with its fields in the
    /// positions the design gives them, a missing list is empty, and a
    /// malformed record is named by its list and position.
    #[test]
    fn parse_filtered_reads_every_record_kind() {
        let text = concat!(
            "{\n",
            r#"  "lines": [[[1, 4], "scope", "session:abc"], [7, "scope", "user:t"]],"#,
            "\n",
            r#"  "values": [["$.cwd", "system", "filter:system"]],"#,
            "\n",
            r#"  "text": [[5, "/message/content", 4, 14, "pii", "filter:pii"]],"#,
            "\n",
            r#"  "attrs": [["project", "system", "filter:system"]],"#,
            "\n",
            r#"  "later": 1"#,
            "\n}\n",
        );
        let records = parse_filtered(text.as_bytes()).unwrap();
        let record =
            |kind, line, line_end, reference: Option<&str>, span, reason: &str, author: &str| {
                let (span_start, span_end) = match span {
                    Some((start, end)) => (Some(start), Some(end)),
                    None => (None, None),
                };
                FilteredRecord {
                    kind,
                    line,
                    line_end,
                    reference: reference.map(String::from),
                    span_start,
                    span_end,
                    reason: reason.into(),
                    author: author.into(),
                }
            };
        assert_eq!(
            records,
            [
                record(
                    FilteredKind::Line,
                    1,
                    Some(4),
                    None,
                    None,
                    "scope",
                    "session:abc"
                ),
                record(
                    FilteredKind::Line,
                    7,
                    Some(7),
                    None,
                    None,
                    "scope",
                    "user:t"
                ),
                record(
                    FilteredKind::Value,
                    0,
                    None,
                    Some("$.cwd"),
                    None,
                    "system",
                    "filter:system"
                ),
                record(
                    FilteredKind::Text,
                    5,
                    None,
                    Some("/message/content"),
                    Some((4, 14)),
                    "pii",
                    "filter:pii"
                ),
                record(
                    FilteredKind::Attr,
                    0,
                    None,
                    Some("project"),
                    None,
                    "system",
                    "filter:system"
                ),
            ]
        );
        assert!(parse_filtered(b"{}\n").unwrap().is_empty());
        assert!(
            parse_filtered(format_filtered(&[]).as_bytes())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_filtered(format_filtered(&records).as_bytes()).unwrap(),
            records
        );

        let bad = parse_filtered(br#"{"lines": [[1, "scope", "a"]], "text": [[2, "/p", "x"]]}"#)
            .unwrap_err();
        assert!(
            matches!(bad, StoreError::Parse(ref m) if m.contains("text record 0") && m.contains("expected 6 fields")),
            "{bad}"
        );
        let reversed = parse_filtered(br#"{"lines": [[[4, 1], "scope", "a"]]}"#).unwrap_err();
        assert!(
            matches!(reversed, StoreError::Parse(ref m) if m.contains("before its start")),
            "{reversed}"
        );
        let array = parse_filtered(b"[]\n").unwrap_err();
        assert!(
            matches!(array, StoreError::Parse(ref m) if m.contains("expected an object")),
            "{array}"
        );
    }

    /// The writer puts one record per line under its list, and a
    /// single dropped line is a bare number where a run is a pair.
    #[test]
    fn format_filtered_writes_one_record_per_line() {
        let records = [
            FilteredRecord {
                kind: FilteredKind::Line,
                line: 2,
                line_end: Some(2),
                reference: None,
                span_start: None,
                span_end: None,
                reason: "scope".into(),
                author: "user:t".into(),
            },
            FilteredRecord {
                kind: FilteredKind::Line,
                line: 4,
                line_end: Some(6),
                reference: None,
                span_start: None,
                span_end: None,
                reason: "scope".into(),
                author: "user:t".into(),
            },
            FilteredRecord {
                kind: FilteredKind::Text,
                line: 1,
                line_end: None,
                reference: Some("/message/content".into()),
                span_start: Some(6),
                span_end: Some(16),
                reason: "pii".into(),
                author: "filter:pii".into(),
            },
        ];
        assert_eq!(
            format_filtered(&records),
            concat!(
                "{\n",
                "  \"lines\": [\n",
                "    [2,\"scope\",\"user:t\"],\n",
                "    [[4,6],\"scope\",\"user:t\"]\n",
                "  ],\n",
                "  \"values\": [],\n",
                "  \"text\": [\n",
                "    [1,\"/message/content\",6,16,\"pii\",\"filter:pii\"]\n",
                "  ],\n",
                "  \"attrs\": []\n",
                "}\n",
            )
        );
    }

    /// A committed publish serves its records through the session
    /// store; a session added from a native source has none.
    #[test]
    fn filtered_reads_the_committed_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\nb\nc\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish = PublishStore::with_root(&store, tmp.path().join("publish"));
        let init = publish.init(&original).unwrap();
        let layout = publish.layout(&init.id);
        fs::write(layout.files_dir().join("session.jsonl"), "a\nc\n").unwrap();
        fs::write(
            layout.filtered_file(),
            r#"{"lines": [[2, "scope", "user:t"]], "text": [[3, "/text", 0, 10, "pii", "filter:pii"]]}"#,
        )
        .unwrap();
        let staged = layout.read_filtered().unwrap();
        assert_eq!(staged.len(), 2);

        let commit = publish.apply(&init.id).unwrap();
        let committed = SessionStore::from(&store).filtered(&commit).unwrap();
        assert_eq!(committed, staged);
        assert_eq!(committed[0].kind, FilteredKind::Line);
        assert_eq!((committed[0].line, committed[0].line_end), (2, Some(2)));
        assert_eq!(committed[1].reference.as_deref(), Some("/text"));

        let none = SessionStore::from(&store)
            .filtered(&outcome.commit_sha)
            .unwrap();
        assert!(none.is_empty());
    }

    /// `abort` deletes the staging directory and the definition and
    /// nothing else; a second abort of the same id is `ObjectNotFound`.
    #[test]
    fn abort_deletes_the_staging_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, _fsck) = open_store(tmp.path());
        let mut session = crate::session::tests::fake("s1", &[("session.jsonl", "a\n")]);
        let outcome = SessionStore::from(&store)
            .add(&crate::session::tests::FakeDriver, &mut session)
            .unwrap();
        let original = SessionStore::from(&store)
            .at_commit(&outcome.commit_sha)
            .unwrap();
        let publish_dir = tmp.path().join("publish");
        let publish = PublishStore::with_root(&store, &publish_dir);
        let a = publish.init(&original).unwrap();
        let b = publish.init(&original).unwrap();
        fs::write(publish.definition_file(&a.id), b"{}\n").unwrap();
        fs::write(publish.definition_file(&b.id), b"{}\n").unwrap();

        publish.abort(&a.id).unwrap();
        assert!(!publish_dir.join(&a.id).exists());
        assert!(!publish.definition_file(&a.id).exists());
        assert!(publish_dir.join(&b.id).exists());
        assert!(publish.definition_file(&b.id).exists());
        assert_eq!(publish.list().unwrap().len(), 1);
        assert_eq!(SessionStore::from(&store).iter().unwrap().count(), 1);

        let again = publish.abort(&a.id).unwrap_err();
        assert!(matches!(again, StoreError::ObjectNotFound(ref id) if *id == a.id));
    }
}
