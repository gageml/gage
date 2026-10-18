//! `native_message_text(source, query [, snippet_len])` and
//! `message_text(query [, snippet_len])` --- full-text search over
//! message text.
//!
//! Each returns `(url, session_id, line, type, subtype, score,
//! snippet)`, best match first, one row per matching message. The
//! native function searches a source's own index through its driver;
//! `message_text` searches the store's index, which this module
//! maintains over the live sessions at their tips: a session whose tip
//! moved is re-read through its driver and re-indexed, and a session
//! gone from the store is dropped. A search reconciles first under a
//! try-lock, and on contention searches the committed index as it
//! stands. `LIMIT n` is pushed through to the search; without one the
//! default cap is [`DEFAULT_LIMIT`].

use std::any::Any;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use datafusion::arrow::array::{
    Array, Float32Builder, Int64Array, Int64Builder, StringArray, StringBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::ScalarValue;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_core::config::default_cache_root;
use gage_index::{LockMode, MessageRow, TextIndex, corpus_slug};
use gage_registry::driver::DriverRegistry;
use gage_session::{MessageTextHit, SourceOptions};
use gage_store::Store;

use crate::native::string_literal;
use crate::rows::{RowCache, message_rows};
use crate::scope::SessionSet;

/// Default cap when no `LIMIT` is supplied. An explicit limit passes
/// through verbatim.
pub const DEFAULT_LIMIT: usize = 100;

/// Argument-list display strings for `\df` in the repl
pub const NATIVE_MESSAGE_TEXT_ARGS: &str = "source text, query text [, snippet_len integer]";
pub const MESSAGE_TEXT_ARGS: &str = "query text [, snippet_len integer]";

/// The scheme of a stored session's URL: `gage:<store path>/<id>`
const STORE_URL_SCHEME: &str = "gage";

pub fn message_text_schema() -> SchemaRef {
    MESSAGE_TEXT_SCHEMA.clone()
}

static MESSAGE_TEXT_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("url", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("line", DataType::Int64, false),
        Field::new("type", DataType::Utf8, true),
        Field::new("subtype", DataType::Utf8, true),
        Field::new("score", DataType::Float32, false),
        Field::new("snippet", DataType::Utf8, false),
    ]))
});

/// `native_message_text(source, query [, snippet_len])`
#[derive(Debug)]
pub struct NativeMessageTextFn {
    /// Read the runtime's own in-flight agent sessions too
    pub show_agents: bool,
    /// Where the opened source keeps its derived state; `None` is the
    /// driver's default root
    pub cache_root: Option<PathBuf>,
}

impl TableFunctionImpl for NativeMessageTextFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let usage = "native_message_text(source, query [, snippet_len])";
        let [source, rest @ ..] = args else {
            return Err(DataFusionError::Plan(format!("usage {usage}")));
        };
        let source = string_literal(source).ok_or_else(|| {
            DataFusionError::Plan(format!("{usage}: source must be a string literal"))
        })?;
        let (query, snippet_len) = query_args(usage, rest)?;
        Ok(Arc::new(MessageTextTable {
            query,
            snippet_len,
            backend: Backend::Native {
                source,
                show_agents: self.show_agents,
                cache_root: self.cache_root.clone(),
            },
        }))
    }
}

/// `message_text(query [, snippet_len])` over the store
pub struct MessageTextFn {
    index: Arc<StoreTextIndex>,
}

impl fmt::Debug for MessageTextFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageTextFn").finish_non_exhaustive()
    }
}

impl MessageTextFn {
    pub fn new(index: Arc<StoreTextIndex>) -> Self {
        Self { index }
    }
}

impl TableFunctionImpl for MessageTextFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let (query, snippet_len) = query_args("message_text(query [, snippet_len])", args)?;
        Ok(Arc::new(MessageTextTable {
            query,
            snippet_len,
            backend: Backend::Store(Arc::clone(&self.index)),
        }))
    }
}

/// The query and snippet length of a call, after any leading source.
fn query_args(usage: &str, args: &[Expr]) -> Result<(String, usize)> {
    let plan = |msg: String| DataFusionError::Plan(format!("{usage}: {msg}"));
    let (query, rest) = match args {
        [query, rest @ ..] => (query, rest),
        [] => return Err(plan("query is required".into())),
    };
    let query =
        string_literal(query).ok_or_else(|| plan("query must be a string literal".into()))?;
    let snippet_len = match rest {
        [] => gage_index::DEFAULT_SNIPPET_CHARS,
        [Expr::Literal(ScalarValue::Int64(Some(n)), _)] if *n > 0 => *n as usize,
        [Expr::Literal(ScalarValue::Int32(Some(n)), _)] if *n > 0 => *n as usize,
        [Expr::Literal(ScalarValue::UInt64(Some(n)), _)] if *n > 0 => *n as usize,
        [Expr::Literal(ScalarValue::UInt32(Some(n)), _)] if *n > 0 => *n as usize,
        [_] => {
            return Err(plan(
                "snippet_len must be a positive integer literal".into(),
            ));
        }
        _ => return Err(plan("too many arguments".into())),
    };
    Ok((query, snippet_len))
}

#[derive(Clone)]
enum Backend {
    Native {
        source: String,
        show_agents: bool,
        cache_root: Option<PathBuf>,
    },
    Store(Arc<StoreTextIndex>),
}

impl Backend {
    /// The hits for `query`, through the source's driver or the
    /// store's index. Blocking: runs the reconcile and the search.
    fn search(&self, query: &str, limit: usize, snippet_len: usize) -> Result<Vec<MessageTextHit>> {
        match self {
            Backend::Native {
                source,
                show_agents,
                cache_root,
            } => {
                let mut options = SourceOptions::new(source);
                options.show_agents(*show_agents);
                if let Some(root) = cache_root {
                    options.cache_root(root);
                }
                // The driver's error is one message with no cause chain
                // worth preserving; a wrapped error would print twice.
                let opened = DriverRegistry::builtin()
                    .open_source(&options)
                    .map_err(|e| DataFusionError::Execution(e.to_string()))?;
                opened
                    .message_text(query, snippet_len, limit)
                    .map_err(|e| DataFusionError::Execution(e.to_string()))
            }
            Backend::Store(index) => {
                index.reconcile(LockMode::Try, &mut |_, _| {})?;
                index.search(query, limit, snippet_len)
            }
        }
    }
}

struct MessageTextTable {
    query: String,
    snippet_len: usize,
    backend: Backend,
}

impl fmt::Debug for MessageTextTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageTextTable")
            .field("query", &self.query)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TableProvider for MessageTextTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        MESSAGE_TEXT_SCHEMA.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let (fetch, default_capped) = match limit {
            Some(n) => (n, false),
            None => (DEFAULT_LIMIT, true),
        };
        let backend = self.backend.clone();
        let query = self.query.clone();
        let snippet_len = self.snippet_len;
        let hits = tokio::task::spawn_blocking(move || backend.search(&query, fetch, snippet_len))
            .await
            .map_err(|e| DataFusionError::Execution(format!("search task failed: {e}")))??;
        if default_capped && hits.len() == fetch {
            tracing::info!(
                "message text search returned the default limit of {DEFAULT_LIMIT} hits; \
                 add an explicit LIMIT for more"
            );
        }
        let batch = hits_batch(&hits)?;
        let mem = MemTable::try_new(MESSAGE_TEXT_SCHEMA.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, &[], None).await
    }
}

fn hits_batch(hits: &[MessageTextHit]) -> Result<RecordBatch> {
    let len = hits.len();
    let mut urls = StringBuilder::with_capacity(len, len * 64);
    let mut session_ids = StringBuilder::with_capacity(len, len * 36);
    let mut lines = Int64Builder::with_capacity(len);
    let mut types = StringBuilder::with_capacity(len, len * 8);
    let mut subtypes = StringBuilder::with_capacity(len, len * 8);
    let mut scores = Float32Builder::with_capacity(len);
    let mut snippets = StringBuilder::with_capacity(len, len * 64);
    for hit in hits {
        urls.append_value(&hit.url);
        session_ids.append_value(&hit.session_id);
        lines.append_value(hit.line);
        types.append_option(hit.type_.as_deref());
        subtypes.append_option(hit.subtype.as_deref());
        scores.append_value(hit.score);
        snippets.append_value(&hit.snippet);
    }
    Ok(RecordBatch::try_new(
        MESSAGE_TEXT_SCHEMA.clone(),
        vec![
            Arc::new(urls.finish()),
            Arc::new(session_ids.finish()),
            Arc::new(lines.finish()),
            Arc::new(types.finish()),
            Arc::new(subtypes.finish()),
            Arc::new(scores.finish()),
            Arc::new(snippets.finish()),
        ],
    )?)
}

/// The store's full-text index: the live sessions at their tips,
/// kept at `<cache root>/text/<slug of the store path>`.
pub struct StoreTextIndex {
    sessions: Arc<SessionSet>,
    store_path: PathBuf,
    cache_root: PathBuf,
}

/// What a reconcile of the store's index did
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreTextOutcome {
    /// Another pass held the lock; nothing was done
    pub skipped: bool,
    /// Live sessions in the store
    pub discovered: usize,
    /// Sessions indexed or re-indexed
    pub indexed: usize,
    /// Sessions dropped from the index
    pub removed: usize,
    pub elapsed_ms: u64,
}

/// The standing of the store's index
#[derive(Debug, Clone)]
pub struct StoreTextStatus {
    pub discovered: usize,
    pub indexed: usize,
    /// Live sessions the index lacks at their current tip
    pub stale: usize,
    pub dir: PathBuf,
    pub bytes: u64,
    pub last_reconcile_ms: Option<i64>,
}

impl StoreTextIndex {
    /// The index of `store`, under `cache_root` or the default root.
    pub fn new(store: Arc<Mutex<Store>>, cache_root: Option<PathBuf>) -> Self {
        let store_path = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .path()
            .to_path_buf();
        Self {
            sessions: Arc::new(SessionSet::all(store)),
            store_path,
            cache_root: cache_root.unwrap_or_else(default_cache_root),
        }
    }

    fn source_url(&self) -> String {
        format!("{STORE_URL_SCHEME}:{}", self.store_path.display())
    }

    fn session_url(&self, id: &str) -> String {
        format!("{}/{id}", self.source_url())
    }

    fn slug(&self) -> String {
        corpus_slug(&self.store_path)
    }

    /// The index directory, whether or not it exists yet.
    pub fn dir(&self) -> PathBuf {
        TextIndex::dir_for(&self.cache_root, &self.slug())
    }

    fn open(&self) -> Result<TextIndex> {
        TextIndex::open(&self.cache_root, &self.slug(), &self.source_url()).map_err(index_error)
    }

    /// Bring the index up to date with the store's live tips.
    /// `on_progress(done, total)` reports the sessions to index or
    /// drop: once with `done == 0`, then after each one.
    pub fn reconcile(
        &self,
        mode: LockMode,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<StoreTextOutcome> {
        let start = std::time::Instant::now();
        let mut text = self.open()?;
        let Some(_guard) = text.lock(mode).map_err(index_error)? else {
            return Ok(StoreTextOutcome {
                skipped: true,
                ..Default::default()
            });
        };
        let tips = self.sessions.sessions(&[])?;
        let live: HashSet<&str> = tips.iter().map(|s| s.id.as_str()).collect();
        let dirty: Vec<_> = tips
            .iter()
            .filter(|s| text.sessions().get(&s.id) != Some(&s.commit))
            .collect();
        let removed: Vec<String> = text
            .sessions()
            .keys()
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        let total = dirty.len() + removed.len();
        let mut done = 0;
        on_progress(done, total);
        let mut outcome = StoreTextOutcome {
            discovered: tips.len(),
            ..Default::default()
        };
        let mut writer = text.writer().map_err(index_error)?;
        for session in dirty {
            // One cache per session: the derived batch is read once
            // here and not retained across the pass.
            let rows = self.sessions.rows(session, &RowCache::new())?;
            let messages = message_rows(rows.as_ref())?;
            writer
                .replace_session(
                    &session.id,
                    &self.session_url(&session.id),
                    &session.commit,
                    message_rows_of(&messages)?,
                )
                .map_err(index_error)?;
            outcome.indexed += 1;
            done += 1;
            on_progress(done, total);
        }
        for id in &removed {
            writer.remove_session(id);
            outcome.removed += 1;
            done += 1;
            on_progress(done, total);
        }
        writer.commit().map_err(index_error)?;
        outcome.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(outcome)
    }

    /// Discard the index and build it again.
    pub fn rebuild(&self, on_progress: &mut dyn FnMut(usize, usize)) -> Result<StoreTextOutcome> {
        TextIndex::remove(&self.cache_root, &self.slug())?;
        self.reconcile(LockMode::Wait, on_progress)
    }

    /// The standing of the index against the store. Read-only: takes
    /// no lock and creates nothing.
    pub fn status(&self) -> Result<StoreTextStatus> {
        let tips = self.sessions.sessions(&[])?;
        let dir = self.dir();
        if !dir.is_dir() {
            return Ok(StoreTextStatus {
                discovered: tips.len(),
                indexed: 0,
                stale: tips.len(),
                dir,
                bytes: 0,
                last_reconcile_ms: None,
            });
        }
        let text = self.open()?;
        let stale = tips
            .iter()
            .filter(|s| text.sessions().get(&s.id) != Some(&s.commit))
            .count();
        let status = text.status();
        Ok(StoreTextStatus {
            discovered: tips.len(),
            indexed: status.indexed,
            stale,
            dir: status.dir,
            bytes: status.bytes,
            last_reconcile_ms: status.last_reconcile_ms,
        })
    }

    /// The top `limit` hits for `query`, best match first.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        snippet_chars: usize,
    ) -> Result<Vec<MessageTextHit>> {
        self.open()?
            .search(query, limit, snippet_chars)
            .map_err(index_error)
    }
}

/// The rows of a `message` batch as the index takes them: columns
/// `line`, `type`, `subtype`, and `text`.
fn message_rows_of(batch: &RecordBatch) -> Result<Vec<MessageRow>> {
    let column = |name: &str| {
        batch
            .column_by_name(name)
            .ok_or_else(|| DataFusionError::Internal(format!("message batch lacks {name}")))
    };
    let lines = column("line")?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| DataFusionError::Internal("message line column type".into()))?;
    let strings = |name: &str| -> Result<&StringArray> {
        column(name)?
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal(format!("message {name} column type")))
    };
    let (types, subtypes, texts) = (strings("type")?, strings("subtype")?, strings("text")?);
    Ok((0..batch.num_rows())
        .filter(|&i| texts.is_valid(i))
        .map(|i| MessageRow {
            line: lines.value(i),
            type_: types.is_valid(i).then(|| types.value(i).to_string()),
            subtype: subtypes.is_valid(i).then(|| subtypes.value(i).to_string()),
            text: texts.value(i).to_string(),
        })
        .collect())
}

fn index_error(e: gage_index::Error) -> DataFusionError {
    match e {
        gage_index::Error::QueryParse(_) => DataFusionError::Plan(e.to_string()),
        other => DataFusionError::External(Box::new(other)),
    }
}

/// The store path a `gage:` session URL names, for callers that route
/// a URL back to its store.
pub fn store_url_path(url: &str) -> Option<&Path> {
    let body = url.strip_prefix(STORE_URL_SCHEME)?.strip_prefix(':')?;
    let (path, _id) = body.rsplit_once('/')?;
    Some(Path::new(path))
}
