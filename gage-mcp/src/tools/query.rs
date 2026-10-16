//! `Query`: SQL over the context the service was configured with.
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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::ipc::reader::FileReader;
use datafusion::arrow::ipc::writer::FileWriter;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_core::config::cache_root;
use gage_core::uuid::new_uuid;
use gage_query::write_yaml_capped;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::json;

use crate::server::GageServer;
use crate::tool::{
    MAX_DESCRIPTION_BYTES, ToolDef, build_tool_meta, description_byte_len, domain_error, success,
};

pub const NAME: &str = "Query";

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/Query.md");
const _: () = assert!(
    description_byte_len(MD) <= MAX_DESCRIPTION_BYTES,
    "Query description exceeds Claude Code's 4096-byte cap",
);

/// The most result text one page holds; a row that alone exceeds it
/// is a page of its own and is reported rather than returned
const PAGE_CAP_BYTES: usize = 45_000;

/// How long a written result stays readable by cursor
const CURSOR_TTL: Duration = Duration::from_secs(60 * 60);

/// Schema metadata key holding the page bounds as a JSON array of
/// row offsets, one per page, closed by the row count
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
                "Query tool is routed but not configured",
                None,
            ));
        };
        execute(&config.context, &QueryCache::new(), request).await
    })
}

/// One call: run new SQL, or read a page of a written result.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryRequest<'a> {
    Sql(&'a str),
    Page { cursor: &'a str, page: usize },
}

/// Exactly one of `sql` and `cursor` is given; `page` goes with
/// `cursor` and defaults to 1.
pub fn parse_request(params: &JsonObject) -> Result<QueryRequest<'_>, McpError> {
    let sql = params.get("sql").and_then(|v| v.as_str());
    let cursor = params.get("cursor").and_then(|v| v.as_str());
    let page = params.get("page").and_then(|v| v.as_u64());
    match (sql, cursor) {
        (Some(_), Some(_)) => Err(McpError::invalid_params(
            "give `sql` or `cursor`, not both",
            None,
        )),
        (None, None) => Err(McpError::invalid_params("missing `sql` or `cursor`", None)),
        (Some(sql), None) => match page {
            Some(_) => Err(McpError::invalid_params(
                "`page` applies to `cursor`, not `sql`",
                None,
            )),
            None => Ok(QueryRequest::Sql(sql)),
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
        QueryRequest::Sql(sql) => {
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
            "To continue, call Query with cursor {cursor} and page {}.",
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
        Self::with_dir(cache_root().join("query"))
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
    use tempfile::TempDir;

    use super::*;

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
                 To continue, call Query with cursor {cursor} and page 2."
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
    }
}
