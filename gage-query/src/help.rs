//! The `help` table: usage notes for the query facility that SQL
//! introspection cannot supply --- the table functions' call
//! signatures, the column and table semantics, the full-text query
//! syntax, and the SQL-dialect limits. A caller sees the topics with
//! `SELECT topic, short FROM help` and reads one with
//! `SELECT full FROM help WHERE topic = '<topic>'`. The same topics and
//! their short lines are listed in the Query tool description; a test in
//! gage-mcp holds the two in sync.

use std::sync::Arc;

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::{MemTable, TableProvider};

/// One `help` row: a lookup `topic`, a one-line `short` for the menu,
/// and the `full` body a caller reads for the topic.
pub struct HelpTopic {
    pub topic: &'static str,
    pub short: &'static str,
    pub full: &'static str,
}

/// Every `help` topic, in menu order.
pub fn help_topics() -> &'static [HelpTopic] {
    HELP
}

/// The `help` table: one row per [`HelpTopic`].
pub(crate) fn help_table() -> Arc<dyn TableProvider> {
    let mut topics = StringBuilder::new();
    let mut shorts = StringBuilder::new();
    let mut fulls = StringBuilder::new();
    for row in HELP {
        topics.append_value(row.topic);
        shorts.append_value(row.short);
        fulls.append_value(row.full);
    }
    let batch = RecordBatch::try_new(
        help_schema(),
        vec![
            Arc::new(topics.finish()),
            Arc::new(shorts.finish()),
            Arc::new(fulls.finish()),
        ],
    )
    .expect("help columns match help_schema");
    Arc::new(
        MemTable::try_new(help_schema(), vec![vec![batch]]).expect("help batch matches schema"),
    )
}

fn help_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("topic", DataType::Utf8, false),
        Field::new("short", DataType::Utf8, false),
        Field::new("full", DataType::Utf8, false),
    ]))
}

const HELP: &[HelpTopic] = &[
    HelpTopic {
        topic: "native",
        short: "session functions that read a source directly, for sessions not in the store",
        full: r#"The session, entry, message, tool_use, and tool_use_file tables hold stored
sessions only. A session named by its native UUID may not be stored.
native_session(source), native_message(source), native_entry(source),
native_tool_use(source), and native_tool_use_file(source) read the source
directly. The argument is required: '' names the default source, 'claude:' the
default Claude Code source, 'claude:/path' a projects directory.

native_session returns (id, title, project, mtime, content_format, size, model,
message_count, line_count, input_tokens, output_tokens, cached_tokens, cost_usd,
summary, is_empty): mtime and size are the session file's, where the stored
session table has created, modified, and the stored content size; the derived
columns and summary are the same as the stored table's. The other four return
their stored tables' columns and key on the native UUID: id in native_session,
session_id in the rest.

A stored session's origin ends in its native UUID, so
SELECT id FROM session WHERE origin LIKE '%/3fefde6c%' tells whether a native
session is already stored."#,
    },
    HelpTopic {
        topic: "message_text",
        short: "full-text message search and its query syntax",
        full: r#"message_text(query [, snippet_len]) searches the text of every stored session's
messages; native_message_text(source, query [, snippet_len]) searches a source's
sessions, with source as the native functions take it. Each returns (url,
session_id, line, type, subtype, score, snippet), one row per matching message,
best match first. url names the session across every source and store
(claude:<root>/<native id>, gage:<store>/<id>); session_id is the id the corpus
uses, so a row joins to message or to native_message(source) on (session_id,
line). Matched terms in snippet are wrapped in «guillemets»; snippet_len caps
its length, default 200. LIMIT n reaches the search; without one the cap is 100
rows. The search reconciles its index first, so a message written since the last
query is found. message_text is absent from a scan-scoped context.

Query syntax:
- Plain terms: refactor cache matches either term
- Require all: refactor AND cache, or +refactor +cache
- Exclude: refactor -cache
- Phrase: "prepared plan"
- Field terms: type:assistant, subtype:tool_use, session_id:abc123
- Combine: +telemetry +type:assistant

Hit with its full message:
SELECT m.session_id, m.line, m.type, t.snippet, m.text
FROM message_text('prepared plan') t
JOIN message m ON m.session_id = t.session_id AND m.line = t.line
ORDER BY t.score DESC LIMIT 20;

Sessions ranked by hits:
SELECT session_id, count(*) AS hits FROM message_text('telemetry')
GROUP BY session_id ORDER BY hits DESC;"#,
    },
    HelpTopic {
        topic: "blame",
        short: "the session call that wrote each line of a file",
        full: r#"native_blame(source, file [, lines]) and blame(file [, lines]) return one row per
contiguous line range and candidate call: (line, line_end, session_id,
session_lines, tool_use_id, tool_name, timestamp, rank, match). line_end is null
for a single line. native_blame reads a native source and blame the stored
sessions. file is a path; lines is optional and selects lines as a number (42)
or as text ('10-20,42'). A line may have several candidate calls; rank 1 is the
strongest, and match names the evidence: patch (the call's recorded patch added
the line), input (its input carries the line and the driver judged it wrote the
file), command (its input carries the line, write unjudged), or neighbor (both
adjacent lines are patch matches of this call). An unattributed line yields no
row. session_lines is the selection to read, from the human prompt before the
call through the call. Filter WHERE rank = 1 for one attribution per line.

SELECT line, line_end, session_id, session_lines, tool_name, match
FROM native_blame('', '/path/to/file.rs', '10-20') WHERE rank = 1;"#,
    },
    HelpTopic {
        topic: "json",
        short: "reading raw, input_raw, result_raw, and content_schema",
        full: r#"raw on entry and message, and input_raw and result_raw on tool_use (and their
native forms), are JSON text in the driver's native shape. content_schema
(content_format, name, schema) describes that shape as a JSON Schema, one row per
content format and native field. Read it before reading into raw:
SELECT name, schema FROM content_schema WHERE content_format = 'claude-jsonl 1';

A path is valid for one content format. Scope rows through session.content_format
(or native_session.content_format) first; on rows of another format the path
yields NULL without an error.

json_extract(json, path) returns one value as text: the path is $ then .key and
[index] steps. A string comes back unquoted, any other value as its JSON text, a
missing step or JSON null as SQL NULL. CAST the text for arithmetic. The -> and
->> operators with string keys also work, as raw->'message'->>'model'. raw is
already text: raw::json and raw::jsonb fail with "Unsupported SQL type". A
comparison directly after ->> binds the key first; parenthesize the extraction
or use json_extract.

In claude-jsonl 1, one API response is written as one row per content block and
every row repeats the response's usage. Sum usage per response, not per row:
SELECT sum(out) AS output_tokens FROM
 (SELECT max(CAST(json_extract(raw, '$.message.usage.output_tokens') AS BIGINT)) AS out
  FROM native_message('claude:')
  WHERE session_id = '<NATIVE_ID>' AND type = 'assistant'
  GROUP BY json_extract(raw, '$.message.id'));"#,
    },
    HelpTopic {
        topic: "summary",
        short: "the summary JSON fields and token totals",
        full: r#"The derived attributes of a session are typed columns on session and on
native_session's result: title, model, message_count, line_count, is_empty,
input_tokens, output_tokens, cached_tokens, and cost_usd. Each is NULL when the
driver has no such fact or the session could not be read. The summary JSON
column carries the same values again, with whatever else the driver derived;
read a driver field with json_extract(summary, '$.cache_read_input_tokens'). A
session that could not be read has NULL in every derived column and a summary of
{"read_error": "<why>"}. The token totals count each API response once:
input_tokens is every prompt token sent, cached or not; output_tokens every
token generated, reasoning included; cached_tokens the part of input served from
cache. cost_usd is the harness's reported figure, absent when it reports none.
Use these before reading usage out of raw."#,
    },
    HelpTopic {
        topic: "tool_use_file",
        short: "the write, read, and cite file-touch rows",
        full: r#"tool_use_file (session_id, tool_use_id, path, type) is one row per tool call per
path it names. type is write when the record proves the call wrote the path,
read when the record proves it did not, and cite when a shell command names the
path and the record does not say. The sessions that touched a file are
SELECT DISTINCT session_id FROM tool_use_file WHERE path = '<path>'; add
AND type = 'write' for the sessions that wrote it."#,
    },
    HelpTopic {
        topic: "notes",
        short: "the note, session_note, and note_doc tables",
        full: r#"note (id, name, target, author, value, text, metadata, created, modified, scan,
work_id). value is the note's JSON text; text is the plain-text form alone and is
null for a structured note. target names what the note is on: session:<id>,
session:<id>#<lines>, or attachment:<id>. scan is the scan the note was written
during. Each name has a row in note_doc (note_name, doc, written_by) describing
what notes of that name hold. session_note (session_id, note_id, lines) is the
session-targeted notes; lines is the cited line ranges as text, for example
831-838 or 211-282,317, null when the note cites no lines."#,
    },
    HelpTopic {
        topic: "issues",
        short: "issue_report, related_issue, and the issue tables",
        full: r#"issue_report('<id>') renders one issue as markdown, the full report with its
evidence. related_issue('<id>' [, min_score]) lists the issues most similar to
one issue, best match first, for finding duplicates. The stored issue data is in
issue (one row per live issue), issue_event (one row per change entry across
them), issue_evidence (issue_id, note_id) its cited notes, and session_issue the
issues raised on a session."#,
    },
    HelpTopic {
        topic: "scans",
        short: "the scan cohort, task, agent, and log tables",
        full: r#"scan_session (scan_id, session_num, session_id) links a scan to the sessions it
covered, its cohort, in dataset member order. A scan with zero notes still has
its cohort rows here, so this is also the existence check for a scan id.

scan_note (scan_id, note_id, carried) links a scan to the notes in its visible
set. carried is false when the scan wrote the note and true when the note
predates the scan and was carried forward. The notes a scan produced:
SELECT n.* FROM note n JOIN scan_note sc ON sc.note_id = n.id
WHERE sc.scan_id = '<SCAN_ID>' AND NOT sc.carried;

scan_issue (scan_id, issue_id) links a scan to the issues it wrote.

scan_task (scan_id, num, scanner, task, status, started, stopped, worked_ms,
selected) is one row per task of a scan; num is the task's position in the plan;
status is pending, started, completed, failed, or canceled. scan_task_agent
(scan_id, scanner, task, session_id, native_id, exit_code, is_error, stop_reason,
turns, duration_ms, cost_usd, result) is one row per agent a task ran; session_id
is the agent's stored session, native_id its native UUID, result the harness's
result message verbatim. An agent still running has null result columns.

scan_log (scan_id, name, text) is one row per log a scan wrote, at most three:
name is out (what the scanners printed), err (Gage's status lines and task
failures), or records (the tracing records, one line each). Read a failed scan's
err first: SELECT text FROM scan_log WHERE scan_id = '<SCAN_ID>' AND name = 'err';"#,
    },
    HelpTopic {
        topic: "links",
        short: "the commit-bearing link tables",
        full: r#"note_target_link, issue_evidence_link, scan_note_link, scan_issue_link,
scan_dataset_link, dataset_session_link, and dataset_attachment_link are the
forms of the link tables that carry the commit of each end (note_commit,
issue_commit, session_commit, and so on). Use them only when a specific commit
matters; the short forms (note_target, issue_evidence, and the rest) join on
ids."#,
    },
    HelpTopic {
        topic: "system_cols",
        short: "the row-addressing system columns",
        full: r#"A context that serves a program also exposes system columns: id_display,
id_prefix, locator, commit, and the *_commit columns. They address a row rather
than describe it. Leave them out of a listing unless a commit is the point."#,
    },
    HelpTopic {
        topic: "limitations",
        short: "DataFusion dialect limits to plan around",
        full: r#"- Correlated scalar subqueries in the SELECT list are not implemented. A query
  like SELECT i.line, (SELECT m.text FROM message m WHERE m.line > i.line ORDER
  BY m.line LIMIT 1) FROM interrupts i fails with "Physical plan does not support
  logical expression ScalarSubquery". Use a ROW_NUMBER() windowed CTE instead.
- Two or more correlated aggregate subqueries in one SELECT list fail with
  "Ambiguous reference to unqualified field". One such subquery works. Join a
  (SELECT scan_id, count(*) ... GROUP BY scan_id) derived table per count.
- No any_value aggregate. To carry a representative value through a GROUP BY, add
  the column to the GROUP BY or use min(...) / max(...)."#,
    },
    HelpTopic {
        topic: "scopes",
        short: "what a scan-scoped context restricts",
        full: r#"A scan-scoped Query tool may also be constrained to specific sessions and, per
session, to a line range. entry, message, and tool_use then return only in-range
rows, even though the SQL carries no such predicate. Rows outside the scope do
not exist from the tool's point of view; do not infer their absence from the
corpus."#,
    },
];
