//! Full-text search over message text: one Tantivy index per corpus
//! at `<cache root>/text/<slug>`.
//!
//! The directory holds `manifest.json`, the reconcile lock, and the
//! Tantivy index under `index/`. The manifest names the format
//! version, the tokenizer chain, the corpus's source URL, and the
//! fingerprint of every session indexed; the owner of the corpus diffs
//! its enumeration against those fingerprints and writes the
//! difference through a [`TextWriter`]. A manifest of another format
//! or chain, or an index that fails to open, discards the directory,
//! and the next pass rebuilds it.
//!
//! A document is one message: `url` and `session_id` (raw terms),
//! `line`, `type`, `subtype`, and the indexed `text`. The tokenizer
//! splits on non-alphanumeric characters, drops tokens over 40 bytes,
//! and lowercases. Bare query terms are OR-combined; `AND`, `+term`,
//! `-term`, phrases, and `field:value` narrow a search.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::FileExt;
use gage_session::MessageTextHit;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    Value as _,
};
use tantivy::snippet::{Snippet, SnippetGenerator};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

/// Index format version: the document schema and the manifest shape.
/// A directory recording another version is discarded.
pub const FORMAT_VERSION: u32 = 1;

/// The tokenizer chain, recorded in the manifest. A directory
/// recording another chain is discarded.
pub const TOKENIZER_CHAIN: &str = "simple+remove_long:40+lowercase";

/// Default character cap on generated snippets.
pub const DEFAULT_SNIPPET_CHARS: usize = 200;

const TOKENIZER_NAME: &str = "gage_default";
const MAX_TOKEN_LEN: usize = 40;
const CATEGORY_DIR: &str = "text";
const INDEX_DIR: &str = "index";
const MANIFEST_FILE: &str = "manifest.json";
const LOCK_FILE: &str = "reconcile.lock";
const WRITER_HEAP_BYTES: usize = 64_000_000;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Tantivy(tantivy::TantivyError),
    Json(serde_json::Error),
    QueryParse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Tantivy(e) => write!(f, "text index error: {e}"),
            Error::Json(e) => write!(f, "manifest error: {e}"),
            Error::QueryParse(e) => write!(f, "invalid text search query: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<tantivy::TantivyError> for Error {
    fn from(e: tantivy::TantivyError) -> Self {
        Error::Tantivy(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// How to take the reconcile lock. A query uses `Try` and searches
/// the committed index when another pass holds the lock; explicit
/// maintenance uses `Wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Try,
    Wait,
}

/// One message of a session, as the owner of a corpus hands it to
/// [`TextWriter::replace_session`].
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub line: i64,
    pub type_: Option<String>,
    pub subtype: Option<String>,
    pub text: String,
}

/// The directory's record: the format of its index, the corpus it
/// indexes, and the fingerprint of every session in it. The session
/// map is written after the index commit, so a crash between the two
/// understates the index and the next pass re-indexes the difference.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format_version: u32,
    tokenizer_chain: String,
    source: String,
    last_reconcile_ms: i64,
    sessions: BTreeMap<String, String>,
}

impl Manifest {
    fn empty(source: &str) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            tokenizer_chain: TOKENIZER_CHAIN.to_string(),
            source: source.to_string(),
            last_reconcile_ms: 0,
            sessions: BTreeMap::new(),
        }
    }

    fn is_current(&self) -> bool {
        self.format_version == FORMAT_VERSION && self.tokenizer_chain == TOKENIZER_CHAIN
    }
}

/// The full-text index of one corpus.
pub struct TextIndex {
    dir: PathBuf,
    index: Index,
    fields: Fields,
    manifest: Manifest,
}

#[derive(Clone, Copy)]
struct Fields {
    url: Field,
    session: Field,
    line: Field,
    type_: Field,
    subtype: Field,
    text: Field,
}

impl TextIndex {
    /// The directory of corpus `slug` under `cache_root`.
    pub fn dir_for(cache_root: &Path, slug: &str) -> PathBuf {
        cache_root.join(CATEGORY_DIR).join(slug)
    }

    /// Open the index of the corpus `slug`, whose sessions are read
    /// from `source`, creating it when absent. A directory of another
    /// format, with an unreadable manifest, or whose index fails to
    /// open is discarded and recreated empty.
    pub fn open(cache_root: &Path, slug: &str, source: &str) -> Result<Self> {
        let dir = Self::dir_for(cache_root, slug);
        if dir.is_dir() {
            match Self::open_existing(&dir) {
                Ok(index) => return Ok(index),
                Err(e) => {
                    tracing::info!(dir = %dir.display(), "discarding text index: {e}");
                    std::fs::remove_dir_all(&dir)?;
                }
            }
        }
        std::fs::create_dir_all(dir.join(INDEX_DIR))?;
        let index = Index::create_in_dir(dir.join(INDEX_DIR), schema())?;
        let manifest = Manifest::empty(source);
        write_manifest(&dir.join(MANIFEST_FILE), &manifest)?;
        Self::assemble(dir, index, manifest)
    }

    fn open_existing(dir: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(dir.join(MANIFEST_FILE))?;
        let manifest: Manifest = serde_json::from_str(&text)?;
        if !manifest.is_current() {
            return Err(Error::Io(io::Error::other(format!(
                "format {} with chain {} is not the current format",
                manifest.format_version, manifest.tokenizer_chain
            ))));
        }
        let index = Index::open_in_dir(dir.join(INDEX_DIR))?;
        Self::assemble(dir.to_path_buf(), index, manifest)
    }

    fn assemble(dir: PathBuf, index: Index, manifest: Manifest) -> Result<Self> {
        index
            .tokenizers()
            .register(TOKENIZER_NAME, build_tokenizer());
        let actual = index.schema();
        let fields = Fields {
            url: actual.get_field("url")?,
            session: actual.get_field("session_id")?,
            line: actual.get_field("line")?,
            type_: actual.get_field("type")?,
            subtype: actual.get_field("subtype")?,
            text: actual.get_field("text")?,
        };
        Ok(Self {
            dir,
            index,
            fields,
            manifest,
        })
    }

    /// Remove the directory of corpus `slug`, if present. The next
    /// `open` starts it empty.
    pub fn remove(cache_root: &Path, slug: &str) -> io::Result<()> {
        match std::fs::remove_dir_all(Self::dir_for(cache_root, slug)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The fingerprint recorded for each indexed session.
    pub fn sessions(&self) -> &BTreeMap<String, String> {
        &self.manifest.sessions
    }

    pub fn status(&self) -> TextIndexStatus {
        TextIndexStatus {
            dir: self.dir.clone(),
            bytes: dir_size(&self.dir),
            indexed: self.manifest.sessions.len(),
            last_reconcile_ms: (self.manifest.last_reconcile_ms > 0)
                .then_some(self.manifest.last_reconcile_ms),
        }
    }

    /// Take the reconcile lock. `None` means another pass holds it
    /// and `mode` was `Try`. The lock is released when the guard
    /// drops.
    pub fn lock(&self, mode: LockMode) -> Result<Option<LockGuard>> {
        let process = process_lock(&self.dir);
        let process_guard = match mode {
            LockMode::Wait => process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            LockMode::Try => match process.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            },
        };
        let file = File::create(self.dir.join(LOCK_FILE))?;
        match mode {
            LockMode::Wait => FileExt::lock(&file)?,
            LockMode::Try => match FileExt::try_lock(&file) {
                Ok(()) => {}
                Err(fs4::TryLockError::WouldBlock) => return Ok(None),
                Err(fs4::TryLockError::Error(e)) => return Err(e.into()),
            },
        }
        Ok(Some(LockGuard {
            _process: process_guard,
            file,
        }))
    }

    /// A writer over the index. Changes reach readers at `commit`.
    pub fn writer(&mut self) -> Result<TextWriter<'_>> {
        let writer = self.index.writer(WRITER_HEAP_BYTES)?;
        Ok(TextWriter {
            index: self,
            writer,
            changed: false,
        })
    }

    /// The top `limit` hits for `query`, best match first, each with
    /// a snippet of at most `snippet_chars` characters.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        snippet_chars: usize,
    ) -> Result<Vec<MessageTextHit>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.fields.text]);
        let query = parser
            .parse_query(query)
            .map_err(|e| Error::QueryParse(e.to_string()))?;
        let mut snippets = SnippetGenerator::create(&searcher, &*query, self.fields.text)?;
        snippets.set_max_num_chars(snippet_chars);
        let top = searcher.search(&query, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let text = |field: Field| {
                doc.get_first(field)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            hits.push(MessageTextHit {
                url: text(self.fields.url).unwrap_or_default(),
                session_id: text(self.fields.session).unwrap_or_default(),
                line: doc
                    .get_first(self.fields.line)
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default(),
                type_: text(self.fields.type_),
                subtype: text(self.fields.subtype),
                score,
                snippet: format_snippet(&snippets.snippet_from_doc(&doc)),
            });
        }
        Ok(hits)
    }
}

/// The reconcile lock of one index, held until dropped.
pub struct LockGuard {
    _process: MutexGuard<'static, ()>,
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // The OS releases the lock with the file; an unlock failure
        // leaves nothing to act on.
        #[allow(clippy::let_underscore_must_use)]
        let _ = FileExt::unlock(&self.file);
    }
}

/// The on-disk standing of one index.
#[derive(Debug, Clone)]
pub struct TextIndexStatus {
    pub dir: PathBuf,
    pub bytes: u64,
    /// Sessions the manifest records as indexed
    pub indexed: usize,
    pub last_reconcile_ms: Option<i64>,
}

/// Pending changes to one index. Documents are keyed by session: a
/// session is replaced whole, its old documents deleted by the
/// `session_id` term.
pub struct TextWriter<'a> {
    index: &'a mut TextIndex,
    writer: IndexWriter,
    changed: bool,
}

impl TextWriter<'_> {
    /// Replace the documents of `session_id` with `rows`, recording
    /// `fingerprint` as the state indexed. `url` is the session's URL,
    /// carried on every document.
    pub fn replace_session(
        &mut self,
        session_id: &str,
        url: &str,
        fingerprint: &str,
        rows: impl IntoIterator<Item = MessageRow>,
    ) -> Result<()> {
        let fields = self.index.fields;
        self.delete_documents(session_id);
        for row in rows {
            let mut doc = TantivyDocument::default();
            doc.add_text(fields.url, url);
            doc.add_text(fields.session, session_id);
            doc.add_i64(fields.line, row.line);
            if let Some(t) = row.type_ {
                doc.add_text(fields.type_, t);
            }
            if let Some(s) = row.subtype {
                doc.add_text(fields.subtype, s);
            }
            doc.add_text(fields.text, row.text);
            self.writer.add_document(doc)?;
        }
        self.index
            .manifest
            .sessions
            .insert(session_id.to_string(), fingerprint.to_string());
        self.changed = true;
        Ok(())
    }

    /// Remove the documents and the manifest entry of `session_id`.
    pub fn remove_session(&mut self, session_id: &str) {
        self.delete_documents(session_id);
        self.index.manifest.sessions.remove(session_id);
        self.changed = true;
    }

    fn delete_documents(&mut self, session_id: &str) {
        // The opstamp names the pending operation; nothing is read
        // from it.
        #[allow(clippy::let_underscore_must_use)]
        let _ = self
            .writer
            .delete_term(Term::from_field_text(self.index.fields.session, session_id));
    }

    /// Commit the pending changes, then record them in the manifest.
    /// With no changes, only the reconcile time is written.
    pub fn commit(mut self) -> Result<()> {
        if self.changed {
            self.writer.commit()?;
        }
        self.index.manifest.last_reconcile_ms = now_ms();
        write_manifest(&self.index.dir.join(MANIFEST_FILE), &self.index.manifest)
    }
}

fn schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("url", STRING | STORED);
    builder.add_text_field("session_id", STRING | STORED);
    builder.add_i64_field("line", STORED | FAST);
    builder.add_text_field("type", STRING | STORED);
    builder.add_text_field("subtype", STRING | STORED);
    builder.add_text_field(
        "text",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(TOKENIZER_NAME)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
    );
    builder.build()
}

fn build_tokenizer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .build()
}

/// Wrap each highlighted range in guillemets. Source text often holds
/// markdown `**`, so a markdown delimiter would collide; guillemets
/// are rare in code and prose and one codepoint on each side.
fn format_snippet(snippet: &Snippet) -> String {
    const OPEN: &str = "«";
    const CLOSE: &str = "»";
    let fragment = snippet.fragment();
    let mut ranges = snippet.highlighted().to_vec();
    if ranges.is_empty() {
        return fragment.to_string();
    }
    ranges.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(fragment.len() + ranges.len() * (OPEN.len() + CLOSE.len()));
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor || range.end > fragment.len() {
            continue;
        }
        out.push_str(&fragment[cursor..range.start]);
        out.push_str(OPEN);
        out.push_str(&fragment[range.start..range.end]);
        out.push_str(CLOSE);
        cursor = range.end;
    }
    out.push_str(&fragment[cursor..]);
    out
}

/// In-process serialization of passes over one directory. The file
/// lock serializes across processes; this keeps threads of one process
/// from try-lock-failing against their own siblings.
static PROCESS_LOCKS: LazyLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn process_lock(dir: &Path) -> &'static Mutex<()> {
    let mut map = PROCESS_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(dir.to_path_buf())
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(manifest)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn row(line: i64, text: &str) -> MessageRow {
        MessageRow {
            line,
            type_: Some("user".into()),
            subtype: None,
            text: text.into(),
        }
    }

    fn index_with(docs: &[(&str, i64, &str)]) -> (TextIndex, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let mut index = TextIndex::open(root.path(), "corpus", "claude:/c").unwrap();
        let mut writer = index.writer().unwrap();
        let mut by_session: BTreeMap<&str, Vec<MessageRow>> = BTreeMap::new();
        for (sid, line, text) in docs {
            by_session.entry(sid).or_default().push(row(*line, text));
        }
        for (sid, rows) in by_session {
            writer
                .replace_session(sid, &format!("claude:/c/{sid}"), "fp1", rows)
                .unwrap();
        }
        writer.commit().unwrap();
        (index, root)
    }

    #[test]
    fn search_returns_scored_hits_with_marked_snippets() {
        let (index, _root) = index_with(&[
            ("s1", 1, "design decision was made here"),
            ("s1", 2, "nothing relevant"),
            ("s2", 1, "the design rationale and decision matter"),
        ]);
        let hits = index
            .search("design decision", 10, DEFAULT_SNIPPET_CHARS)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score >= hits[1].score);
        assert!(hits.iter().all(|h| h.snippet.contains('«')));
        assert_eq!(hits[0].url, format!("claude:/c/{}", hits[0].session_id));
        assert_eq!(hits[0].type_.as_deref(), Some("user"));
    }

    #[test]
    fn bare_terms_are_or_combined() {
        let (index, _root) = index_with(&[
            ("s1", 1, "alpha and beta together"),
            ("s2", 1, "only alpha here"),
            ("s3", 1, "only beta here"),
            ("s4", 1, "neither term present"),
        ]);
        let any = index
            .search("alpha beta", 10, DEFAULT_SNIPPET_CHARS)
            .unwrap();
        assert_eq!(any.len(), 3);
        assert_eq!(any[0].session_id, "s1");
        let both = index
            .search("alpha AND beta", 10, DEFAULT_SNIPPET_CHARS)
            .unwrap();
        assert_eq!(both.len(), 1);
    }

    #[test]
    fn an_invalid_query_is_an_error() {
        let (index, _root) = index_with(&[("s1", 1, "x")]);
        let err = index.search("\"unterminated", 10, DEFAULT_SNIPPET_CHARS);
        assert!(matches!(err, Err(Error::QueryParse(_))));
    }

    #[test]
    fn limit_caps_hits() {
        let docs: Vec<(&str, i64, &str)> = (0..20).map(|i| ("s1", i, "the term appears")).collect();
        let (index, _root) = index_with(&docs);
        let hits = index.search("term", 5, DEFAULT_SNIPPET_CHARS).unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn replacing_a_session_drops_its_old_documents() {
        let (mut index, _root) = index_with(&[("s1", 1, "first version"), ("s1", 2, "kept")]);
        let mut writer = index.writer().unwrap();
        writer
            .replace_session("s1", "claude:/c/s1", "fp2", [row(1, "second version")])
            .unwrap();
        writer.commit().unwrap();
        assert!(index.search("first", 10, 100).unwrap().is_empty());
        assert_eq!(index.search("second", 10, 100).unwrap().len(), 1);
        assert_eq!(index.sessions()["s1"], "fp2");

        let mut writer = index.writer().unwrap();
        writer.remove_session("s1");
        writer.commit().unwrap();
        assert!(index.search("second", 10, 100).unwrap().is_empty());
        assert!(index.sessions().is_empty());
    }

    #[test]
    fn the_manifest_survives_a_reopen_and_names_the_source() {
        let (index, root) = index_with(&[("s1", 1, "x")]);
        drop(index);
        let reopened = TextIndex::open(root.path(), "corpus", "claude:/c").unwrap();
        assert_eq!(reopened.sessions()["s1"], "fp1");
        assert_eq!(reopened.search("x", 10, 100).unwrap().len(), 1);
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(reopened.dir().join(MANIFEST_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["source"], "claude:/c");
        assert_eq!(reopened.dir(), root.path().join("text").join("corpus"));
    }

    #[test]
    fn another_format_discards_the_directory() {
        let (index, root) = index_with(&[("s1", 1, "x")]);
        let dir = index.dir().to_path_buf();
        drop(index);
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"{"format_version":0,"tokenizer_chain":"other","source":"claude:/c","last_reconcile_ms":1,"sessions":{"s1":"fp1"}}"#,
        )
        .unwrap();
        let reopened = TextIndex::open(root.path(), "corpus", "claude:/c").unwrap();
        assert!(reopened.sessions().is_empty());
        assert!(reopened.search("x", 10, 100).unwrap().is_empty());
    }

    #[test]
    fn a_try_lock_yields_to_a_held_lock() {
        let (index, _root) = index_with(&[]);
        let held = index.lock(LockMode::Wait).unwrap();
        assert!(held.is_some());
        assert!(index.lock(LockMode::Try).unwrap().is_none());
        drop(held);
        assert!(index.lock(LockMode::Try).unwrap().is_some());
    }
}
