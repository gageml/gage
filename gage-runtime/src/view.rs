//! Prompt-ready session views: `pages` splits a session into
//! full-fidelity pages and `roadmap` renders the whole session as an
//! abbreviated facsimile. Both are prompt-construction helpers for
//! scanners that deliver session content in the agent's initial prompt
//! instead of having the agent page it through tool results.

use std::collections::HashMap;

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use datafusion::common::ScalarValue;
use gage_claude::entry::block_to_text;
use rune::runtime::{Protocol, Ref, Value};
use rune::{Any, ContextError, Module};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value as Json;

use crate::error::Error;
use crate::scan::Session;
use crate::state::current_scan_ctx;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function_meta(pages)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionPages| async move {
        do_session_pages(q).await
    })?;

    m.function_meta(roadmap)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionRoadmap| async move {
        do_session_roadmap(q).await
    })?;

    Ok(())
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<SessionPages>()?;
    m.ty::<SessionRoadmap>()?;
    m.ty::<Page>()?;
    Ok(())
}

/// One full-fidelity page of a session: a contiguous run of messages
/// rendered as prompt text. `start`/`end` are the inclusive session
/// line numbers the page covers; `index` is 1-based.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Page {
    #[rune(get)]
    pub index: u64,
    #[rune(get)]
    pub start: u64,
    #[rune(get)]
    pub end: u64,
    #[rune(get)]
    pub text: String,
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionPages {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    lines: Option<(u64, u64)>,
    #[rune(skip)]
    opts: Value,
}

/// Split the session into full-fidelity pages for per-page agent
/// prompts. Options: `size` — target page size in chars (a single
/// message larger than `size` becomes its own oversized page).
#[rune::function(instance)]
fn pages(session: Ref<Session>, opts: Value) -> SessionPages {
    SessionPages {
        session_id: session.id.clone(),
        lines: session.range.map(|r| (r.start, r.end)),
        opts,
    }
}

async fn do_session_pages(q: SessionPages) -> super::Result<Vec<Page>> {
    let opts: PageOpts = parse_opts(&q.opts, "pages")?;
    let rows = fetch_rows(&q.session_id, q.lines).await?;
    Ok(build_pages(&rows, &opts))
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PageOpts {
    /// Target page size in chars. Pages close at the first message
    /// boundary past this size.
    pub size: usize,
}

impl Default for PageOpts {
    fn default() -> Self {
        PageOpts { size: 120_000 }
    }
}

fn build_pages(rows: &[MessageRow], opts: &PageOpts) -> Vec<Page> {
    let mut pages: Vec<Page> = Vec::new();
    let mut text = String::new();
    let mut span: Option<(u64, u64)> = None;
    for row in rows {
        let rendered = render_full(row);
        if !text.is_empty() && text.chars().count() + rendered.chars().count() > opts.size {
            flush_page(&mut pages, &mut text, &mut span);
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&rendered);
        span = match span {
            Some((start, _)) => Some((start, row.line)),
            None => Some((row.line, row.line)),
        };
    }
    flush_page(&mut pages, &mut text, &mut span);
    pages
}

fn flush_page(pages: &mut Vec<Page>, text: &mut String, span: &mut Option<(u64, u64)>) {
    if let Some((start, end)) = span.take() {
        pages.push(Page {
            index: pages.len() as u64 + 1,
            start,
            end,
            text: std::mem::take(text),
        });
    }
}

/// Render one message at full fidelity: a `[L<line> <type> <subtype>]`
/// header, any out-of-band ide tags, then the message text.
fn render_full(row: &MessageRow) -> String {
    let mut s = header(row);
    if let Some(tags) = &row.ide_tags {
        s.push('\n');
        s.push_str(tags);
    }
    if !row.text.is_empty() {
        s.push('\n');
        s.push_str(&row.text);
    }
    s
}

fn header(row: &MessageRow) -> String {
    match &row.subtype {
        Some(sub) if *sub != row.type_ => format!("[L{} {} {}]", row.line, row.type_, sub),
        _ => format!("[L{} {}]", row.line, row.type_),
    }
}

#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionRoadmap {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    lines: Option<(u64, u64)>,
    #[rune(skip)]
    opts: Value,
}

/// Render the whole session as an abbreviated roadmap for an agent
/// prompt. Every message appears under its `[L<line> …]` header with
/// its content truncated by per-kind char caps; truncation markers
/// carry the omitted char counts so the agent can judge what to
/// retrieve in full.
///
/// Options (all char caps): `user_text`, `meta_text`,
/// `assistant_text`, `thinking_head`, `thinking_tail`, `tool_input`,
/// `result_line`.
#[rune::function(instance)]
fn roadmap(session: Ref<Session>, opts: Value) -> SessionRoadmap {
    SessionRoadmap {
        session_id: session.id.clone(),
        lines: session.range.map(|r| (r.start, r.end)),
        opts,
    }
}

async fn do_session_roadmap(q: SessionRoadmap) -> super::Result<String> {
    let opts: RoadmapOpts = parse_opts(&q.opts, "roadmap")?;
    let rows = fetch_rows(&q.session_id, q.lines).await?;
    Ok(build_roadmap(&rows, &opts))
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoadmapOpts {
    pub user_text: usize,
    pub meta_text: usize,
    pub assistant_text: usize,
    pub thinking_head: usize,
    pub thinking_tail: usize,
    pub tool_input: usize,
    pub result_line: usize,
}

impl Default for RoadmapOpts {
    fn default() -> Self {
        RoadmapOpts {
            user_text: 500,
            meta_text: 200,
            assistant_text: 500,
            thinking_head: 300,
            thinking_tail: 200,
            tool_input: 200,
            result_line: 200,
        }
    }
}

fn build_roadmap(rows: &[MessageRow], opts: &RoadmapOpts) -> String {
    // tool_use id → tool name, for labeling tool_result lines
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        out.push(render_abbrev(row, opts, &mut tool_names));
    }
    out.join("\n\n")
}

/// Render one message abbreviated: header, an ide-tags size marker,
/// then each content block reduced per its kind's cap.
fn render_abbrev(
    row: &MessageRow,
    opts: &RoadmapOpts,
    tool_names: &mut HashMap<String, String>,
) -> String {
    let mut s = header(row);
    if let Some(tags) = &row.ide_tags {
        s.push_str(&format!("\n<ide tags: {} chars>", tags.chars().count()));
    }
    let body = match serde_json::from_str::<Json>(&row.raw) {
        Ok(raw) => match raw.get("message").and_then(|m| m.get("content")) {
            Some(Json::Array(blocks)) => blocks
                .iter()
                .map(|b| render_block(b, row, opts, tool_names))
                .filter(|b| !b.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            Some(Json::String(text)) => truncate_chars(text, text_cap(row, opts)),
            _ => truncate_chars(&row.text, opts.meta_text),
        },
        // The message table serves raw straight from the session
        // JSONL, so a parse failure is unexpected; surface it in the
        // rendering rather than dropping the message.
        Err(e) => format!(
            "<raw unparsed: {e}>\n{}",
            truncate_chars(&row.text, opts.meta_text)
        ),
    };
    if !body.is_empty() {
        s.push('\n');
        s.push_str(&body);
    }
    s
}

fn render_block(
    block: &Json,
    row: &MessageRow,
    opts: &RoadmapOpts,
    tool_names: &mut HashMap<String, String>,
) -> String {
    let ty = block.get("type").and_then(Json::as_str).unwrap_or("");
    match ty {
        "text" => {
            let text = block.get("text").and_then(Json::as_str).unwrap_or("");
            truncate_chars(text, text_cap(row, opts))
        }
        "thinking" => {
            let text = block.get("thinking").and_then(Json::as_str).unwrap_or("");
            format!(
                "thinking: {}",
                head_tail(text, opts.thinking_head, opts.thinking_tail)
            )
        }
        "tool_use" => {
            let name = block.get("name").and_then(Json::as_str).unwrap_or("?");
            if let Some(id) = block.get("id").and_then(Json::as_str) {
                tool_names.insert(id.to_string(), name.to_string());
            }
            let input = block.get("input").map(Json::to_string).unwrap_or_default();
            format!(
                "tool_use {name}: {}",
                truncate_chars(&input, opts.tool_input)
            )
        }
        "tool_result" => {
            let name = block
                .get("tool_use_id")
                .and_then(Json::as_str)
                .and_then(|id| tool_names.get(id))
                .map(|n| format!(" {n}"))
                .unwrap_or_default();
            let error = match block.get("is_error").and_then(Json::as_bool) {
                Some(true) => " (error)",
                _ => "",
            };
            let text = block_to_text(block);
            format!(
                "tool_result{name}{error}: {}",
                first_line(&text, opts.result_line)
            )
        }
        "image" => "<image>".into(),
        "" => String::new(),
        other => format!("<{other}>"),
    }
}

/// The text-block cap for a message's role: `meta_text` for meta user
/// messages and non-conversation rows, else the role's cap.
fn text_cap(row: &MessageRow, opts: &RoadmapOpts) -> usize {
    match (row.type_.as_str(), row.subtype.as_deref()) {
        ("user", Some("meta")) => opts.meta_text,
        ("user", _) => opts.user_text,
        ("assistant", _) => opts.assistant_text,
        _ => opts.meta_text,
    }
}

/// First `cap` chars, with an omitted-count marker when truncated.
fn truncate_chars(s: &str, cap: usize) -> String {
    let total = s.chars().count();
    if total <= cap {
        return s.to_string();
    }
    let head: String = s.chars().take(cap).collect();
    format!("{head}… [+{} chars]", total - cap)
}

/// First `head` and last `tail` chars with the omitted middle counted.
fn head_tail(s: &str, head: usize, tail: usize) -> String {
    let total = s.chars().count();
    if total <= head + tail {
        return s.to_string();
    }
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(total - tail).collect();
    format!("{h}… [{} chars omitted] …{t}", total - head - tail)
}

/// First line of `s` capped at `cap` chars, with the rest counted.
fn first_line(s: &str, cap: usize) -> String {
    let total = s.chars().count();
    let line = s.lines().next().unwrap_or("");
    let shown = line.chars().count().min(cap);
    let head: String = line.chars().take(cap).collect();
    if total <= shown {
        head
    } else {
        format!("{head}… [+{} chars]", total - shown)
    }
}

fn parse_opts<T: DeserializeOwned>(opts: &Value, what: &str) -> super::Result<T> {
    let json = serde_json::to_value(opts)
        .map_err(|e| Error::Args(format!("`{what}` options could not be read: {e}")))?;
    serde_json::from_value(json).map_err(|e| Error::Args(format!("invalid `{what}` options: {e}")))
}

pub(crate) struct MessageRow {
    pub line: u64,
    pub type_: String,
    pub subtype: Option<String>,
    pub text: String,
    pub ide_tags: Option<String>,
    pub raw: String,
}

async fn fetch_rows(session_id: &str, lines: Option<(u64, u64)>) -> super::Result<Vec<MessageRow>> {
    let ctx = current_scan_ctx();
    let df_ctx = &ctx.run.scan_ctx;

    let mut params: Vec<ScalarValue> = vec![ScalarValue::Utf8(Some(session_id.to_string()))];
    let mut clauses = vec!["session_id = $1".to_string()];
    if let Some((start, end)) = lines {
        params.push(ScalarValue::Int64(Some(start as i64)));
        clauses.push(format!("line >= ${}", params.len()));
        params.push(ScalarValue::Int64(Some(end as i64)));
        clauses.push(format!("line <= ${}", params.len()));
    }
    let sql = format!(
        "SELECT * FROM message WHERE {} ORDER BY line",
        clauses.join(" AND ")
    );

    let df = df_ctx
        .sql(&sql)
        .await
        .map_err(|e| Error::Db(e.to_string()))?;
    let df = df
        .with_param_values(params)
        .map_err(|e| Error::Db(e.to_string()))?;
    let batches = df.collect().await.map_err(|e| Error::Db(e.to_string()))?;

    let mut rows = Vec::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let schema = batch.schema();
        let col = |name: &str| batch.column(schema.index_of(name).unwrap());
        let line_arr = col("line").as_any().downcast_ref::<Int64Array>().unwrap();
        let type_arr = col("type").as_any().downcast_ref::<StringArray>().unwrap();
        let subtype_arr = col("subtype")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let text_arr = col("text").as_any().downcast_ref::<StringArray>().unwrap();
        let ide_tags_arr = col("ide_tags")
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let raw_arr = col("raw").as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..batch.num_rows() {
            rows.push(MessageRow {
                line: line_arr.value(i) as u64,
                type_: type_arr.value(i).to_string(),
                subtype: opt_str(subtype_arr, i),
                text: text_arr.value(i).to_string(),
                ide_tags: opt_str(ide_tags_arr, i),
                raw: raw_arr.value(i).to_string(),
            });
        }
    }
    Ok(rows)
}

fn opt_str(arr: &StringArray, i: usize) -> Option<String> {
    if arr.is_null(i) {
        None
    } else {
        Some(arr.value(i).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: u64, type_: &str, subtype: Option<&str>, text: &str, raw: &str) -> MessageRow {
        MessageRow {
            line,
            type_: type_.to_string(),
            subtype: subtype.map(str::to_string),
            text: text.to_string(),
            ide_tags: None,
            raw: raw.to_string(),
        }
    }

    fn user_row(line: u64, text: &str) -> MessageRow {
        let raw = serde_json::json!({
            "type": "user",
            "message": { "content": [{ "type": "text", "text": text }] },
        });
        row(line, "user", Some("text"), text, &raw.to_string())
    }

    #[test]
    fn pages_split_at_message_boundaries() {
        let rows = vec![
            user_row(1, &"a".repeat(50)),
            user_row(2, &"b".repeat(50)),
            user_row(3, &"c".repeat(50)),
        ];
        let pages = build_pages(&rows, &PageOpts { size: 130 });
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].index, pages[0].start, pages[0].end), (1, 1, 2));
        assert_eq!((pages[1].index, pages[1].start, pages[1].end), (2, 3, 3));
        assert!(pages[0].text.contains(&"a".repeat(50)));
        assert!(pages[0].text.contains(&"b".repeat(50)));
        assert!(pages[1].text.contains(&"c".repeat(50)));
    }

    #[test]
    fn pages_keep_oversized_message_whole() {
        let rows = vec![user_row(1, &"a".repeat(500)), user_row(2, "small")];
        let pages = build_pages(&rows, &PageOpts { size: 100 });
        assert_eq!(pages.len(), 2);
        assert!(pages[0].text.contains(&"a".repeat(500)));
    }

    #[test]
    fn pages_render_headers_and_full_text() {
        let pages = build_pages(&[user_row(7, "hello")], &PageOpts::default());
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].text, "[L7 user text]\nhello");
    }

    #[test]
    fn pages_empty_session_yields_no_pages() {
        assert!(build_pages(&[], &PageOpts::default()).is_empty());
    }

    #[test]
    fn roadmap_truncates_user_text() {
        let long = "x".repeat(600);
        let out = build_roadmap(&[user_row(1, &long)], &RoadmapOpts::default());
        assert!(out.starts_with("[L1 user text]\n"));
        assert!(out.contains(&"x".repeat(500)));
        assert!(out.contains("… [+100 chars]"));
        assert!(!out.contains(&"x".repeat(501)));
    }

    #[test]
    fn roadmap_reduces_tool_traffic() {
        let big_input = "y".repeat(400);
        let use_raw = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "Bash",
                "input": { "command": big_input },
            }] },
        });
        let result_raw = serde_json::json!({
            "type": "user",
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "is_error": true,
                "content": "line one\nline two\nline three",
            }] },
        });
        let rows = vec![
            row(1, "assistant", Some("tool_use"), "", &use_raw.to_string()),
            row(2, "user", Some("tool_result"), "", &result_raw.to_string()),
        ];
        let out = build_roadmap(&rows, &RoadmapOpts::default());
        assert!(out.contains("tool_use Bash: "));
        assert!(out.contains("… [+"));
        assert!(!out.contains(&"y".repeat(300)));
        assert!(out.contains("tool_result Bash (error): line one… [+"));
        assert!(!out.contains("line two"));
    }

    #[test]
    fn roadmap_thinking_keeps_head_and_tail() {
        let thinking = format!("HEAD{}TAIL", "m".repeat(1000));
        let raw = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "thinking", "thinking": thinking }] },
        });
        let rows = vec![row(1, "assistant", Some("thinking"), "", &raw.to_string())];
        let out = build_roadmap(&rows, &RoadmapOpts::default());
        assert!(out.contains("thinking: HEAD"));
        assert!(out.contains("chars omitted"));
        assert!(out.ends_with("TAIL"));
    }

    #[test]
    fn roadmap_notes_ide_tags_without_content() {
        let mut r = user_row(1, "body");
        r.ide_tags = Some("<system-reminder>secret stuff</system-reminder>".to_string());
        let out = build_roadmap(&[r], &RoadmapOpts::default());
        assert!(out.contains("<ide tags: 47 chars>"));
        assert!(!out.contains("secret stuff"));
    }

    #[test]
    fn truncate_chars_is_char_boundary_safe() {
        assert_eq!(truncate_chars("日本語のテキスト", 3), "日本語… [+5 chars]");
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn head_tail_short_input_unchanged() {
        assert_eq!(head_tail("abc", 300, 200), "abc");
    }

    #[test]
    fn first_line_counts_remaining_chars() {
        assert_eq!(first_line("one\nrest", 10), "one… [+5 chars]");
        assert_eq!(first_line("only", 10), "only");
        assert_eq!(first_line("", 10), "");
    }

    #[test]
    fn opts_reject_unknown_fields() {
        let err = serde_json::from_value::<PageOpts>(serde_json::json!({ "sizes": 1 }));
        assert!(err.is_err(), "unknown field should be rejected");
    }

    #[test]
    fn view_builders_leave_caller_values_readable() {
        use crate::datetime::DateTime;
        use crate::scan::Range;
        use std::path::PathBuf;

        let session = Session {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            modified: DateTime::from_millis(0),
            src: PathBuf::from("/tmp/session.jsonl"),
            range: Some(Range { start: 3, end: 9 }),
        };
        let sv = rune::to_value(session).unwrap();
        let mut obj = rune::runtime::Object::new();
        obj.insert(
            rune::alloc::String::try_from("size").unwrap(),
            rune::to_value(64_i64).unwrap(),
        )
        .unwrap();
        let opts = rune::to_value(obj).unwrap();

        let q = SessionPages {
            session_id: sv.borrow_ref::<Session>().unwrap().id.clone(),
            lines: Some((3, 9)),
            opts: opts.clone(),
        };
        let parsed: PageOpts = parse_opts(&q.opts, "pages").unwrap();
        assert_eq!(parsed.size, 64);

        // The caller's values are reads, not takes: both stay readable
        let s = sv.borrow_ref::<Session>().unwrap();
        assert_eq!(s.id, "11111111-1111-1111-1111-111111111111");
        let o = opts.borrow_ref::<rune::runtime::Object>().unwrap();
        assert!(o.contains_key("size"));
    }
}
