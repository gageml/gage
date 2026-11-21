//! `query`: SQL over the context the service was configured with,
//! or over what the call names in its place, a native source, another
//! store, or the objects fetched from a remote, each of which is read
//! and never written.
//!
//! Every result is written once to the query cache as an Arrow IPC
//! file named by a cursor, and every call returns one page of it. A
//! `sql` call runs the query, writes the file, and returns page one
//! with the cursor; a `cursor` call reads a page of a result already
//! written. Page bounds are fixed when the file is written and stored
//! in the file's schema metadata, so every page of one result is cut
//! from the same rows regardless of the query's order. Each call
//! sweeps files older than the TTL before its own work, so a scan
//! process and an interactive server clear each other's leftovers.

use std::fs::{self, File};
use std::future::Future;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::ipc::reader::FileReader;
use datafusion::arrow::ipc::writer::FileWriter;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_core::config::default_cache_root;
use gage_core::uuid::new_uuid;
use gage_query::{ContextBuilder, write_yaml_capped};
use gage_store::Store;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::json;

use crate::server::GageServer;
use crate::service::{QueryConfig, SourceConfig};
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};

pub const NAME: &str = "query";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/query.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "query description exceeds Claude Code's 4096-byte cap",
);

/// The most result text one page holds; a row that alone exceeds it
/// is a page of its own and is reported rather than returned.
const PAGE_CAP_BYTES: usize = 45_000;

/// How long a written result stays readable by cursor
const CURSOR_TTL: Duration = Duration::from_secs(60 * 60);

/// Schema metadata key holding the page bounds as a JSON array of
/// row offsets, one per page, closed by the row count.
const PAGES_KEY: &str = "gage.pages";

const FILE_EXT: &str = "arrow";
const TMP_EXT: &str = "tmp";

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new_dyn(
        build_tool_meta(MD),
        |ctx: ToolCallContext<'_, GageServer>| Box::pin(handle(ctx)),
    )
}

fn handle(
    ctx: ToolCallContext<'_, GageServer>,
) -> Pin<Box<dyn Future<Output = Result<CallToolResult, McpError>> + Send + '_>> {
    Box::pin(async move {
        let params = ctx.arguments.unwrap_or_default();
        let request = parse_request(&params)?;
        let Some(config) = &ctx.service.config().query else {
            return Err(McpError::internal_error(
                "query tool is routed but not configured",
                None,
            ));
        };
        let context = context_for(config, &request).await?;
        execute(&context, &QueryCache::new(), request).await
    })
}

/// The context `request` runs on: the configured one, for a `native`,
/// `gage_store`, or `stored_remote` call a fresh one over what it
/// names, or for a `publish` call a fresh one scoped to the pending
/// publishes, listed on each call so the set is current. A call where
/// the configuration does not provide for it is refused.
async fn context_for(
    config: &QueryConfig,
    request: &QueryRequest<'_>,
) -> Result<Arc<SessionContext>, McpError> {
    match request {
        QueryRequest::Sql(_) | QueryRequest::Page { .. } => Ok(Arc::clone(&config.context)),
        QueryRequest::NativeSql { native, .. } => {
            let config = source_config(config, "native")?;
            native_context(config, native).map(Arc::new)
        }
        QueryRequest::StoreSql { gage_store, .. } => {
            let config = source_config(config, "gage_store")?;
            store_context(config, gage_store).await.map(Arc::new)
        }
        QueryRequest::StoredRemoteSql {
            remote, gage_store, ..
        } => {
            let config = source_config(config, "stored_remote")?;
            stored_remote_context(config, *gage_store, remote)
                .await
                .map(Arc::new)
        }
        QueryRequest::PublishSql(_) => publish_context(config).await.map(Arc::new),
    }
}

/// What a call reading `param` in place of the main store needs, or
/// the refusal of a tool not configured for one.
fn source_config<'a>(config: &'a QueryConfig, param: &str) -> Result<&'a SourceConfig, McpError> {
    config.source.as_ref().ok_or_else(|| {
        McpError::invalid_params(
            format!("`{param}` does not apply here: this tool reads its own scope alone"),
            None,
        )
    })
}

/// A context over the native source `native` in place of the main
/// store. A native source has no write path. One that cannot be
/// opened is an invalid request naming why.
fn native_context(config: &SourceConfig, native: &str) -> Result<SessionContext, McpError> {
    ContextBuilder::new(None)
        .cache_root(&config.cache_root)
        .build_source(native)
        .map_err(|e| McpError::invalid_params(format!("native {native}: {e}"), None))
}

/// A context over the store at `gage_store` in place of the main
/// store, as `GAGE_STORE` names one for a command. The store is
/// opened read only, so a store of another version is refused and not
/// migrated. One that cannot be opened is an invalid request naming
/// why.
async fn store_context(
    config: &SourceConfig,
    gage_store: &str,
) -> Result<SessionContext, McpError> {
    let store = Store::open_read_only(Path::new(gage_store), &config.cache_root)
        .map_err(|e| McpError::invalid_params(format!("gage_store {gage_store}: {e}"), None))?;
    Ok(ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await)
}

/// A context over the objects fetched from `remote` in place of a
/// store's own: the store at `gage_store` when the call names one,
/// else the main store. The store is opened on the remote's tracking
/// refs, which are read and never written, and nothing is fetched. A
/// remote that is not configured or was never fetched is an invalid
/// request naming why.
async fn stored_remote_context(
    config: &SourceConfig,
    gage_store: Option<&str>,
    remote: &str,
) -> Result<SessionContext, McpError> {
    let path = gage_store.map_or(config.store.as_path(), Path::new);
    let store = Store::open_stored_remote(path, &config.cache_root, remote)
        .map_err(|e| McpError::invalid_params(format!("stored_remote {remote}: {e}"), None))?;
    Ok(ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await)
}

async fn publish_context(config: &QueryConfig) -> Result<SessionContext, McpError> {
    let Some(publish) = &config.publish else {
        return Err(McpError::invalid_params(
            "`publish` does not apply here: pending publish sessions are not readable in this \
             context",
            None,
        ));
    };
    let sessions = (publish.sessions)().map_err(|reason| {
        McpError::internal_error(format!("pending publish sessions: {reason}"), None)
    })?;
    Ok(ContextBuilder::new(Some(Arc::clone(&publish.store)))
        .external(sessions)
        .skip_system_cols()
        .build()
        .await)
}

/// One call: run new SQL over the sessions in scope, over a native
/// source, over another store, over the objects fetched from a remote
/// of the main store or of another, or over the pending publishes, or
/// read a page of a written result.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryRequest<'a> {
    Sql(&'a str),
    NativeSql {
        native: &'a str,
        sql: &'a str,
    },
    StoreSql {
        gage_store: &'a str,
        sql: &'a str,
    },
    StoredRemoteSql {
        remote: &'a str,
        gage_store: Option<&'a str>,
        sql: &'a str,
    },
    PublishSql(&'a str),
    Page {
        cursor: &'a str,
        page: usize,
    },
}

/// Exactly one of `sql` and `cursor` is given; `page` goes with
/// `cursor` and defaults to 1; `publish`, `native`, `gage_store`,
/// and `stored_remote` go with `sql`. `native` and `publish` each
/// stand alone; `gage_store` and `stored_remote` go together, the
/// remote then being one of that store.
pub fn parse_request(params: &JsonObject) -> Result<QueryRequest<'_>, McpError> {
    let sql = params.get("sql").and_then(|v| v.as_str());
    let cursor = params.get("cursor").and_then(|v| v.as_str());
    let page = params.get("page").and_then(|v| v.as_u64());
    let publish = params.get("publish").and_then(|v| v.as_bool());
    let native = params.get("native").and_then(|v| v.as_str());
    let stored_remote = params.get("stored_remote").and_then(|v| v.as_str());
    let gage_store = params.get("gage_store").and_then(|v| v.as_str());
    if publish.is_some() && sql.is_none() {
        return Err(McpError::invalid_params(
            "`publish` applies to `sql`, not `cursor`",
            None,
        ));
    }
    for (name, given) in [
        ("native", native),
        ("stored_remote", stored_remote),
        ("gage_store", gage_store),
    ] {
        if given.is_some() && sql.is_none() {
            return Err(McpError::invalid_params(
                format!("`{name}` applies to `sql`, not `cursor`"),
                None,
            ));
        }
    }
    if gage_store == Some("") {
        return Err(McpError::invalid_params(
            "`gage_store` is empty: give the path of a store",
            None,
        ));
    }
    let scopes = [
        native.is_some(),
        stored_remote.is_some() || gage_store.is_some(),
        publish == Some(true),
    ];
    if scopes.into_iter().filter(|given| *given).count() > 1 {
        return Err(McpError::invalid_params(
            "`native` and `publish` each stand alone: only `gage_store` and `stored_remote` \
             go together",
            None,
        ));
    }
    match (sql, cursor) {
        (Some(_), Some(_)) => Err(McpError::invalid_params(
            "`sql` and `cursor` were both given. To read another page of a stored result, send \
             only `cursor` and `page`; to run a new query, send only `sql`",
            None,
        )),
        (None, None) => Err(McpError::invalid_params("missing `sql` or `cursor`", None)),
        (Some(sql), None) => match page {
            Some(_) => Err(McpError::invalid_params(
                "`page` applies to `cursor`, not `sql`",
                None,
            )),
            None if publish == Some(true) => Ok(QueryRequest::PublishSql(sql)),
            None => Ok(match (native, stored_remote, gage_store) {
                (Some(native), _, _) => QueryRequest::NativeSql { native, sql },
                (None, Some(remote), gage_store) => QueryRequest::StoredRemoteSql {
                    remote,
                    gage_store,
                    sql,
                },
                (None, None, Some(gage_store)) => QueryRequest::StoreSql { gage_store, sql },
                (None, None, None) => QueryRequest::Sql(sql),
            }),
        },
        (None, Some(cursor)) => match page {
            Some(0) => Err(McpError::invalid_params("`page` starts at 1", None)),
            Some(page) => Ok(QueryRequest::Page {
                cursor,
                page: usize::try_from(page).map_err(|e| {
                    McpError::invalid_params(format!("`page` is out of range: {e}"), None)
                })?,
            }),
            None => Ok(QueryRequest::Page { cursor, page: 1 }),
        },
    }
}

/// Serve one request against the cache. SQL and execution errors,
/// an unknown or expired cursor, and a page past the end are results
/// the model reads; a cache I/O failure or a serialization failure
/// is a protocol error.
pub async fn execute(
    context: &SessionContext,
    cache: &QueryCache,
    request: QueryRequest<'_>,
) -> Result<CallToolResult, McpError> {
    execute_with_cap(context, cache, request, PAGE_CAP_BYTES).await
}

async fn execute_with_cap(
    context: &SessionContext,
    cache: &QueryCache,
    request: QueryRequest<'_>,
    cap_bytes: usize,
) -> Result<CallToolResult, McpError> {
    cache.sweep();
    match request {
        QueryRequest::Sql(sql)
        | QueryRequest::NativeSql { sql, .. }
        | QueryRequest::StoreSql { sql, .. }
        | QueryRequest::StoredRemoteSql { sql, .. }
        | QueryRequest::PublishSql(sql) => {
            let df = match context.sql(sql).await {
                Ok(df) => df,
                Err(e) => return Ok(domain_error(format!("SQL error: {e}"))),
            };
            let batches = match df.collect().await {
                Ok(b) => b,
                Err(e) => return Ok(domain_error(format!("query execution error: {e}"))),
            };
            let batches: Vec<_> = batches
                .iter()
                .filter(|b| b.num_rows() > 0)
                .cloned()
                .collect();
            if batches.is_empty() {
                return Ok(success("0 rows"));
            }
            let pages = page_bounds(&batches, cap_bytes)?;
            let cursor = cache
                .write(&batches, &pages)
                .map_err(|e| McpError::internal_error(format!("query cache error: {e}"), None))?;
            render_page(&batches, &pages, &cursor, 1, cap_bytes)
        }
        QueryRequest::Page { cursor, page } => {
            let result = if is_cursor(cursor) {
                cache.read(cursor).map_err(|e| {
                    McpError::internal_error(format!("query cache error: {e}"), None)
                })?
            } else {
                None
            };
            let Some(result) = result else {
                return Ok(domain_error(format!(
                    "cursor {cursor} is unknown or expired; re-run the query"
                )));
            };
            render_page(&result.batches, &result.pages, cursor, page, cap_bytes)
        }
    }
}

/// Cut the rows into pages of at most `cap_bytes` of YAML each. A row
/// that alone exceeds the cap is a page by itself.
fn page_bounds(batches: &[RecordBatch], cap_bytes: usize) -> Result<Vec<Range<usize>>, McpError> {
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    let mut pages = Vec::new();
    let mut start = 0;
    while start < total {
        let rest = slice_rows(batches, start..total);
        let mut buf = yaml_open();
        let fit = write_yaml_capped(&mut buf, &rest, cap_bytes).map_err(|e| {
            McpError::internal_error(format!("YAML serialization error: {e}"), None)
        })?;
        let len = fit.max(1);
        pages.push(start..start + len);
        start += len;
    }
    Ok(pages)
}

fn render_page(
    batches: &[RecordBatch],
    pages: &[Range<usize>],
    cursor: &str,
    page: usize,
    cap_bytes: usize,
) -> Result<CallToolResult, McpError> {
    let count = pages.len();
    let Some(range) = page.checked_sub(1).and_then(|i| pages.get(i)) else {
        return Ok(domain_error(format!(
            "cursor {cursor} has {count} pages; page {page} does not exist"
        )));
    };
    let rows = slice_rows(batches, range.clone());
    let mut buf = yaml_open();
    let written = write_yaml_capped(&mut buf, &rows, cap_bytes)
        .map_err(|e| McpError::internal_error(format!("YAML serialization error: {e}"), None))?;
    if written == 0 {
        let msg = json!({
            "error": format!("row {} of cursor {cursor} exceeds the page cap", range.start + 1),
            "cap_bytes": cap_bytes,
            "suggestion": "SELECT substr(text, 1, 800) or substr(raw, 1, 800) \
                           instead of the full column, or omit wide columns \
                           (text, raw) entirely.",
        })
        .to_string();
        return Ok(domain_error(msg));
    }
    buf.extend_from_slice(b"\n```\n");
    let yaml = String::from_utf8(buf)
        .map_err(|e| McpError::internal_error(format!("UTF-8 error: {e}"), None))?;
    let total = pages.last().map_or(0, |r| r.end);
    let mut text = format!(
        "cursor {cursor}: page {page} of {count}, rows {}-{} of {total}\n{yaml}",
        range.start + 1,
        range.end
    );
    if page < count {
        text.push_str(&format!(
            "Next page: call this tool again with exactly these arguments, and no sql: \
             {{\"cursor\": \"{cursor}\", \"page\": {}}}",
            page + 1
        ));
    }
    Ok(success(text))
}

fn yaml_open() -> Vec<u8> {
    b"```yaml\n".to_vec()
}

/// The rows in `range`, counted across the batches in order.
fn slice_rows(batches: &[RecordBatch], range: Range<usize>) -> Vec<RecordBatch> {
    let mut out = Vec::new();
    let mut offset = 0;
    for batch in batches {
        let batch_range = offset..offset + batch.num_rows();
        let start = range.start.max(batch_range.start);
        let end = range.end.min(batch_range.end);
        if start < end {
            out.push(batch.slice(start - offset, end - start));
        }
        offset = batch_range.end;
    }
    out
}

/// A cursor is a Gage ID: 26 lowercase Crockford characters. The check
/// keeps a cursor argument from naming any other path.
fn is_cursor(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// The directory of written results.
pub struct QueryCache {
    dir: PathBuf,
}

/// A result read back by cursor
struct CachedResult {
    batches: Vec<RecordBatch>,
    pages: Vec<Range<usize>>,
}

impl Default for QueryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryCache {
    /// The cache under the Gage cache root.
    pub fn new() -> Self {
        Self::with_dir(default_cache_root().join("query"))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        QueryCache { dir }
    }

    /// Write the batches as one IPC file under a new cursor, with the
    /// page bounds in the schema metadata. The file appears under its
    /// final name only once complete.
    fn write(&self, batches: &[RecordBatch], pages: &[Range<usize>]) -> io::Result<String> {
        let first = batches
            .first()
            .ok_or_else(|| io::Error::other("a written result has at least one batch"))?;
        fs::create_dir_all(&self.dir)?;
        let cursor = new_uuid();
        let ends: Vec<usize> = pages.iter().map(|r| r.end).collect();
        let mut metadata = first.schema().metadata().clone();
        metadata.insert(PAGES_KEY.to_string(), json!(ends).to_string());
        let schema = Arc::new(Schema::new_with_metadata(
            first.schema().fields().clone(),
            metadata,
        ));
        let tmp = self.path(&cursor, TMP_EXT);
        let file = File::create(&tmp)?;
        let mut writer = FileWriter::try_new(file, &schema).map_err(io::Error::other)?;
        for batch in batches {
            let batch = batch
                .clone()
                .with_schema(Arc::clone(&schema))
                .map_err(io::Error::other)?;
            writer.write(&batch).map_err(io::Error::other)?;
        }
        writer.finish().map_err(io::Error::other)?;
        fs::rename(&tmp, self.path(&cursor, FILE_EXT))?;
        Ok(cursor)
    }

    /// The result under `cursor`, or `None` when no file holds it.
    fn read(&self, cursor: &str) -> io::Result<Option<CachedResult>> {
        let file = match File::open(self.path(cursor, FILE_EXT)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let reader = FileReader::try_new(file, None).map_err(io::Error::other)?;
        let ends: Vec<usize> = reader
            .schema()
            .metadata()
            .get(PAGES_KEY)
            .ok_or_else(|| io::Error::other(format!("cursor {cursor} has no page bounds")))
            .and_then(|s| serde_json::from_str(s).map_err(io::Error::other))?;
        let batches = reader
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        let mut start = 0;
        let pages = ends
            .into_iter()
            .map(|end| {
                let range = start..end;
                start = end;
                range
            })
            .collect();
        Ok(Some(CachedResult { batches, pages }))
    }

    /// Remove every file older than the TTL. A file another sweep
    /// removed first is not an error; any other failure is logged
    /// and the sweep goes on, since the call's own write reports a
    /// cache that is actually unusable.
    fn sweep(&self) {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(dir = %self.dir.display(), error = %e, "query cache sweep");
                return;
            }
        };
        let now = SystemTime::now();
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    tracing::warn!(dir = %self.dir.display(), error = %e, "query cache sweep");
                    continue;
                }
            };
            if let Err(e) = sweep_file(&path, now)
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), error = %e, "query cache sweep");
            }
        }
    }

    fn path(&self, cursor: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{cursor}.{ext}"))
    }
}

fn sweep_file(path: &Path, now: SystemTime) -> io::Result<()> {
    let modified = fs::metadata(path)?.modified()?;
    let expired = now
        .duration_since(modified)
        .is_ok_and(|age| age > CURSOR_TTL);
    if expired {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::arrow::array::{Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::datasource::MemTable;
    use gage_query::help::help_topics;
    use tempfile::TempDir;

    use super::*;
    use crate::service::{PublishConfig, SourceConfig};

    fn context() -> SessionContext {
        let schema = Arc::new(Schema::new(vec![
            Field::new("line", DataType::Int64, false),
            Field::new("text", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["hello", "world"])),
            ],
        )
        .unwrap();
        let ctx = SessionContext::new();
        ctx.register_table(
            "message",
            Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
        )
        .unwrap();
        ctx
    }

    fn cache() -> (TempDir, QueryCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = QueryCache::with_dir(dir.path().join("query"));
        (dir, cache)
    }

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect()
    }

    /// The cursor named on the first line of a page
    fn cursor_of(result: &CallToolResult) -> String {
        let text = text_of(result);
        let rest = text.strip_prefix("cursor ").unwrap();
        rest[..rest.find(':').unwrap()].to_string()
    }

    const ALL: &str = "SELECT line, text FROM message ORDER BY line";

    #[tokio::test]
    async fn rows_render_as_yaml_with_the_cursor() {
        let (_dir, cache) = cache();
        let r = execute(&context(), &cache, QueryRequest::Sql(ALL))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(false));
        let cursor = cursor_of(&r);
        assert!(is_cursor(&cursor), "{cursor}");
        assert_eq!(
            text_of(&r),
            format!(
                "cursor {cursor}: page 1 of 1, rows 1-2 of 2\n\
                 ```yaml\nline: 1\ntext: hello\n---\nline: 2\ntext: world\n\n```\n"
            )
        );
    }

    #[tokio::test]
    async fn no_rows_and_sql_errors_are_results_the_model_reads() {
        let (_dir, cache) = cache();
        let r = execute(
            &context(),
            &cache,
            QueryRequest::Sql("SELECT * FROM message WHERE line > 5"),
        )
        .await
        .unwrap();
        assert_eq!(text_of(&r), "0 rows");
        let r = execute(
            &context(),
            &cache,
            QueryRequest::Sql("SELECT nope FROM message"),
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert!(text_of(&r).starts_with("SQL error: "));
    }

    #[tokio::test]
    async fn a_result_over_the_cap_is_read_page_by_page_through_its_cursor() {
        let (_dir, cache) = cache();
        let ctx = context();
        // One row of YAML is 22 bytes after the 8-byte opener; a cap of
        // 40 holds one row per page
        let first = execute_with_cap(&ctx, &cache, QueryRequest::Sql(ALL), 40)
            .await
            .unwrap();
        let cursor = cursor_of(&first);
        assert_eq!(
            text_of(&first),
            format!(
                "cursor {cursor}: page 1 of 2, rows 1-1 of 2\n\
                 ```yaml\nline: 1\ntext: hello\n\n```\n\
                 Next page: call this tool again with exactly these arguments, and no sql: \
                 {{\"cursor\": \"{cursor}\", \"page\": 2}}"
            )
        );

        let page = |page| QueryRequest::Page {
            cursor: &cursor,
            page,
        };
        let second = execute_with_cap(&ctx, &cache, page(2), 40).await.unwrap();
        assert_eq!(second.is_error, Some(false));
        assert_eq!(
            text_of(&second),
            format!(
                "cursor {cursor}: page 2 of 2, rows 2-2 of 2\n\
                 ```yaml\nline: 2\ntext: world\n\n```\n"
            )
        );

        let again = execute_with_cap(&ctx, &cache, page(1), 40).await.unwrap();
        assert_eq!(text_of(&again), text_of(&first));

        let past = execute_with_cap(&ctx, &cache, page(3), 40).await.unwrap();
        assert_eq!(past.is_error, Some(true));
        assert_eq!(
            text_of(&past),
            format!("cursor {cursor} has 2 pages; page 3 does not exist")
        );
    }

    #[tokio::test]
    async fn a_row_over_the_cap_is_its_own_page_and_is_reported() {
        let (_dir, cache) = cache();
        let r = execute_with_cap(&context(), &cache, QueryRequest::Sql(ALL), 10)
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        let text = text_of(&r);
        assert!(text.contains("row 1 of cursor"), "{text}");
        assert!(text.contains("exceeds the page cap"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_or_malformed_cursor_is_a_result_the_model_reads() {
        let (_dir, cache) = cache();
        for cursor in ["0123456789abcdefghjkmnpqrs", "../../etc/passwd", "x"] {
            let r = execute(&context(), &cache, QueryRequest::Page { cursor, page: 1 })
                .await
                .unwrap();
            assert_eq!(r.is_error, Some(true));
            assert_eq!(
                text_of(&r),
                format!("cursor {cursor} is unknown or expired; re-run the query")
            );
        }
    }

    #[tokio::test]
    async fn a_call_sweeps_results_older_than_the_ttl() {
        let (_dir, cache) = cache();
        let ctx = context();
        let old = cursor_of(&execute(&ctx, &cache, QueryRequest::Sql(ALL)).await.unwrap());
        let fresh = cursor_of(&execute(&ctx, &cache, QueryRequest::Sql(ALL)).await.unwrap());
        let old_path = cache.path(&old, FILE_EXT);
        let stale = fs::File::options().write(true).open(&old_path).unwrap();
        stale
            .set_modified(SystemTime::now() - CURSOR_TTL - Duration::from_secs(60))
            .unwrap();
        drop(stale);
        let leftover = cache.path("0123456789abcdefghjkmnpqrs", TMP_EXT);
        fs::write(&leftover, b"partial").unwrap();
        fs::File::options()
            .write(true)
            .open(&leftover)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();

        let r = execute(
            &ctx,
            &cache,
            QueryRequest::Page {
                cursor: &fresh,
                page: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(false));
        assert!(!old_path.exists(), "expired result was not swept");
        assert!(!leftover.exists(), "expired partial file was not swept");
        assert!(cache.path(&fresh, FILE_EXT).exists());

        let r = execute(
            &ctx,
            &cache,
            QueryRequest::Page {
                cursor: &old,
                page: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[test]
    fn parse_request_takes_sql_or_a_cursor_with_a_page() {
        let object = |v: serde_json::Value| match v {
            serde_json::Value::Object(o) => o,
            _ => unreachable!(),
        };
        assert_eq!(
            parse_request(&object(json!({"sql": "SELECT 1"}))).unwrap(),
            QueryRequest::Sql("SELECT 1")
        );
        assert_eq!(
            parse_request(&object(json!({"cursor": "c"}))).unwrap(),
            QueryRequest::Page {
                cursor: "c",
                page: 1
            }
        );
        assert_eq!(
            parse_request(&object(json!({"cursor": "c", "page": 3}))).unwrap(),
            QueryRequest::Page {
                cursor: "c",
                page: 3
            }
        );
        for bad in [
            json!({}),
            json!({"sql": "SELECT 1", "cursor": "c"}),
            json!({"sql": "SELECT 1", "page": 2}),
            json!({"cursor": "c", "page": 0}),
        ] {
            let e = parse_request(&object(bad.clone())).unwrap_err();
            assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn wire_definition_comes_from_the_md() {
        let meta = build_tool_meta(MD);
        assert_eq!(meta.name, NAME);
        let annotations = meta.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.idempotent_hint, Some(true));
        assert_eq!(meta.input_schema.get("required").unwrap(), &json!([]));
        let properties = meta.input_schema.get("properties").unwrap();
        assert_eq!(properties["sql"]["type"], "string");
        assert_eq!(properties["cursor"]["type"], "string");
        assert_eq!(properties["page"]["type"], "integer");
        assert_eq!(properties["publish"]["type"], "boolean");
        assert_eq!(properties["native"]["type"], "string");
        assert_eq!(properties["stored_remote"]["type"], "string");
        assert_eq!(properties["gage_store"]["type"], "string");
    }

    /// The help menu embedded in the description stays in sync with the
    /// help table: the same topics, in order, with the same short lines.
    #[test]
    fn help_menu_matches_the_help_table() {
        let meta = build_tool_meta(MD);
        let body = meta.description.as_deref().expect("query.md has a body");
        let menu: Vec<(&str, &str)> = body
            .lines()
            .filter_map(|line| line.split_once(": "))
            .filter(|(topic, _)| {
                !topic.is_empty() && topic.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
            .collect();
        let table: Vec<(&str, &str)> = help_topics().iter().map(|t| (t.topic, t.short)).collect();
        assert_eq!(menu, table);
    }

    /// `publish` goes with `sql`: true selects the pending publishes,
    /// false or absent the configured scope, and with a cursor it is
    /// an invalid request.
    #[test]
    fn parse_request_reads_publish_with_sql() {
        let object = |v: serde_json::Value| match v {
            serde_json::Value::Object(o) => o,
            _ => unreachable!(),
        };
        assert_eq!(
            parse_request(&object(json!({"sql": "SELECT 1", "publish": true}))).unwrap(),
            QueryRequest::PublishSql("SELECT 1")
        );
        assert_eq!(
            parse_request(&object(json!({"sql": "SELECT 1", "publish": false}))).unwrap(),
            QueryRequest::Sql("SELECT 1")
        );
        let e = parse_request(&object(json!({"cursor": "c", "publish": true}))).unwrap_err();
        assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    /// A `publish` call runs on a fresh context over the pending
    /// publishes the configured listing returns, which has the
    /// session tables and no other store table; any other call runs
    /// on the configured context. Without a publish listing the
    /// `publish` call is refused.
    #[tokio::test]
    async fn publish_calls_run_on_an_external_scoped_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.git");
        gage_store::init(&path).unwrap();
        let store = Arc::new(std::sync::Mutex::new(
            gage_store::Store::open_with_cache_root(&path, &dir.path().join("cache")).unwrap(),
        ));
        let configured = Arc::new(context());
        let config = QueryConfig {
            context: Arc::clone(&configured),
            publish: Some(PublishConfig {
                store,
                sessions: Arc::new(|| Ok(Vec::new())),
            }),
            source: None,
        };

        let same = context_for(&config, &QueryRequest::Sql("SELECT 1"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&same, &configured));

        let scoped = context_for(&config, &QueryRequest::PublishSql("SELECT 1"))
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&scoped, &configured));
        let names = |sql: &str| {
            let scoped = Arc::clone(&scoped);
            let sql = sql.to_string();
            async move {
                let batches = scoped.sql(&sql).await.unwrap().collect().await.unwrap();
                batches.iter().map(|b| b.num_rows()).sum::<usize>()
            }
        };
        assert_eq!(names("SELECT id FROM session").await, 0);
        assert_eq!(
            names("SELECT table_name FROM information_schema.tables WHERE table_name = 'note'")
                .await,
            0
        );
        assert_eq!(
            names(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_name IN ('session', 'entry', 'message', 'tool_use', \
                                      'tool_use_file', 'session_publish_change')"
            )
            .await,
            6
        );

        let without = QueryConfig {
            context: configured,
            publish: None,
            source: None,
        };
        let refused = context_for(&without, &QueryRequest::PublishSql("SELECT 1"))
            .await
            .map(|_| ());
        let Err(e) = refused else {
            panic!("a publish call without a store was accepted");
        };
        assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    /// `native`, `gage_store`, and `stored_remote` go with `sql` and
    /// name what the SQL reads. `gage_store` and `stored_remote` go
    /// together; any of them with a cursor, any other pairing, and an
    /// empty `gage_store` are an invalid request.
    #[test]
    fn parse_request_reads_the_named_source_with_sql() {
        let object = |v: serde_json::Value| match v {
            serde_json::Value::Object(o) => o,
            _ => unreachable!(),
        };
        assert_eq!(
            parse_request(&object(json!({"sql": "SELECT 1", "stored_remote": "acme"}))).unwrap(),
            QueryRequest::StoredRemoteSql {
                remote: "acme",
                gage_store: None,
                sql: "SELECT 1"
            }
        );
        assert_eq!(
            parse_request(&object(
                json!({"sql": "SELECT 1", "gage_store": "/srv/zuni.git"})
            ))
            .unwrap(),
            QueryRequest::StoreSql {
                gage_store: "/srv/zuni.git",
                sql: "SELECT 1"
            }
        );
        assert_eq!(
            parse_request(&object(json!(
                {"sql": "SELECT 1", "gage_store": "/srv/zuni.git", "stored_remote": "acme"}
            )))
            .unwrap(),
            QueryRequest::StoredRemoteSql {
                remote: "acme",
                gage_store: Some("/srv/zuni.git"),
                sql: "SELECT 1"
            }
        );
        assert_eq!(
            parse_request(&object(
                json!({"sql": "SELECT 1", "native": "claude:", "publish": false})
            ))
            .unwrap(),
            QueryRequest::NativeSql {
                native: "claude:",
                sql: "SELECT 1"
            }
        );
        for bad in [
            json!({"cursor": "c", "native": "claude:"}),
            json!({"cursor": "c", "stored_remote": "acme"}),
            json!({"sql": "SELECT 1", "native": "claude:", "publish": true}),
            json!({"sql": "SELECT 1", "stored_remote": "acme", "publish": true}),
            json!({"sql": "SELECT 1", "native": "claude:", "stored_remote": "acme"}),
            json!({"cursor": "c", "gage_store": "/srv/zuni.git"}),
            json!({"sql": "SELECT 1", "gage_store": "/srv/zuni.git", "publish": true}),
            json!({"sql": "SELECT 1", "gage_store": "/srv/zuni.git", "native": "claude:"}),
            json!({"sql": "SELECT 1", "gage_store": ""}),
        ] {
            let e = parse_request(&object(bad.clone())).unwrap_err();
            assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{bad}");
        }
    }

    /// A `stored_remote` call reads the objects fetched from the named
    /// remote and none of the store's own, and writes no ref. A remote
    /// that is not configured or was never fetched, and a call on a
    /// tool not configured for one, are refused.
    #[tokio::test]
    async fn stored_remote_calls_read_the_fetched_objects_alone() {
        use gage_store::{NoteInput, NoteStore, NoteValue, RemoteStore};

        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let note = |path: &Path, value: &str| {
            gage_store::init(path).unwrap();
            let store = Store::open_with_cache_root(path, &cache_root).unwrap();
            NoteStore::from(&store)
                .create(NoteInput {
                    name: "comment",
                    value: NoteValue::Text(value.into()),
                    author: "user:t",
                    target: None,
                    metadata: None,
                    work_id: None,
                })
                .unwrap();
            store
        };
        let theirs = dir.path().join("theirs.git");
        drop(note(&theirs, "from the remote"));
        let ours = dir.path().join("ours.git");
        let store = note(&ours, "from the main store");
        let remotes = RemoteStore::from(&store);
        remotes
            .add("acme", theirs.to_str().unwrap(), false)
            .unwrap();
        remotes
            .add("idle", theirs.to_str().unwrap(), false)
            .unwrap();
        drop(store);
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&ours)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["fetch", "--quiet", "acme"]);
        let refs_before = git(&["for-each-ref"]);

        let configured = Arc::new(context());
        let config = QueryConfig {
            context: Arc::clone(&configured),
            publish: None,
            source: Some(SourceConfig {
                store: ours.clone(),
                cache_root: cache_root.clone(),
            }),
        };
        let request = QueryRequest::StoredRemoteSql {
            remote: "acme",
            gage_store: None,
            sql: "SELECT value FROM note",
        };
        let named = context_for(&config, &request).await.unwrap();
        assert!(!Arc::ptr_eq(&named, &configured));
        let (_cache_dir, cache) = cache();
        let result = execute(&named, &cache, request).await.unwrap();
        let text = text_of(&result);
        assert!(text.contains("from the remote"), "{text}");
        assert!(!text.contains("from the main store"), "{text}");
        drop(named);
        assert_eq!(git(&["for-each-ref"]), refs_before);

        // `gage_store` names another store to read in place of the
        // configured one, and with `stored_remote` a remote of it.
        let elsewhere = QueryConfig {
            context: Arc::clone(&configured),
            publish: None,
            source: Some(SourceConfig {
                store: dir.path().join("main.git"),
                cache_root: cache_root.clone(),
            }),
        };
        for (remote, expected, absent) in [
            (None, "from the main store", "from the remote"),
            (Some("acme"), "from the remote", "from the main store"),
        ] {
            let gage_store = ours.to_str().unwrap();
            let sql = "SELECT value FROM note";
            let request = match remote {
                None => QueryRequest::StoreSql { gage_store, sql },
                Some(remote) => QueryRequest::StoredRemoteSql {
                    remote,
                    gage_store: Some(gage_store),
                    sql,
                },
            };
            let named = context_for(&elsewhere, &request).await.unwrap();
            let text = text_of(&execute(&named, &cache, request).await.unwrap());
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains(absent), "{text}");
        }
        assert_eq!(git(&["for-each-ref"]), refs_before);
        let missing = dir.path().join("missing.git");
        let refused = context_for(
            &elsewhere,
            &QueryRequest::StoreSql {
                gage_store: missing.to_str().unwrap(),
                sql: "SELECT 1",
            },
        )
        .await
        .map(|_| ());
        let Err(e) = refused else {
            panic!("a gage_store that is not a store was accepted");
        };
        assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(e.message.contains("gage_store"), "{}", e.message);

        for (remote, reason) in [("idle", "gage fetch idle"), ("nope", "remote not found")] {
            let refused = context_for(
                &config,
                &QueryRequest::StoredRemoteSql {
                    remote,
                    gage_store: None,
                    sql: "SELECT 1",
                },
            )
            .await
            .map(|_| ());
            let Err(e) = refused else {
                panic!("stored_remote {remote} was accepted");
            };
            assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(e.message.contains(reason), "{}", e.message);
        }

        let without = QueryConfig {
            context: configured,
            publish: None,
            source: None,
        };
        for request in [
            QueryRequest::StoredRemoteSql {
                remote: "acme",
                gage_store: None,
                sql: "SELECT 1",
            },
            QueryRequest::StoreSql {
                gage_store: ours.to_str().unwrap(),
                sql: "SELECT 1",
            },
            QueryRequest::NativeSql {
                native: "claude:",
                sql: "SELECT 1",
            },
        ] {
            let refused = context_for(&without, &request).await.map(|_| ());
            let Err(e) = refused else {
                panic!("{request:?} on a scoped tool was accepted");
            };
            assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        }
    }

    /// A `native` call serves the named source's sessions under the
    /// session tables, with no store table.
    #[tokio::test]
    async fn native_calls_read_a_driver_source() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("claude").join("projects").join("-w-proj");
        std::fs::create_dir_all(&project).unwrap();
        let id = "aaaaaaaa-0000-0000-0000-000000000001";
        std::fs::write(
            project.join(format!("{id}.jsonl")),
            concat!(
                r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let config = QueryConfig {
            context: Arc::new(context()),
            publish: None,
            source: Some(SourceConfig {
                store: dir.path().join("store.git"),
                cache_root: dir.path().join("cache"),
            }),
        };
        let native = format!("claude:{}", dir.path().join("claude").display());
        let request = QueryRequest::NativeSql {
            native: &native,
            sql: "SELECT id FROM session",
        };
        let named = context_for(&config, &request).await.unwrap();
        let (_cache_dir, cache) = cache();
        let result = execute(&named, &cache, request).await.unwrap();
        assert!(text_of(&result).contains(id), "{}", text_of(&result));
        let absent = execute(&named, &cache, QueryRequest::Sql("SELECT id FROM note"))
            .await
            .unwrap();
        assert_eq!(absent.is_error, Some(true));
    }
}
