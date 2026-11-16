//! The `help` table: usage notes for the query facility that SQL
//! introspection cannot supply --- the table functions' call
//! signatures, the column and table semantics, the full-text query
//! syntax, and the SQL-dialect limits. A caller sees the topics with
//! `SELECT topic, short FROM help` and reads one with
//! `SELECT full FROM help WHERE topic = '<topic>'`. The same topics and
//! their short lines are listed in the query tool description; a test in
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
        topic: "source",
        short: "reading a native source or a remote's fetched objects in place of the main store",
        full: r#"A query reads one source. With none named it is the main store. Two parameters
of the query tool each name another, and the query then reads that source and
nothing of the main store. Give one or the other, not both.

native, or --native on gage query, names a native source:

claude:<path>   the Claude Code sessions under <path>/projects
claude:         the Claude Code sessions in the default location
codex:<path>    the Codex sessions under <path>/sessions
codex:          the Codex sessions in the default location

stored_remote, or --stored-remote on gage query, names a remote of the main
store. The query reads the objects gage fetch brought from that remote, which
are kept apart from the main store's own until gage merge merges them. Every
store table is present, holding the remote's sessions, notes, issues, scans, and
datasets. Nothing is fetched by a query: the rows are the remote as of the last
gage fetch.

A named source is read only. The write tools write the main store alone, and an
id read from another source is not a target for them.

A native source such as claude: holds sessions whether or not they are stored.
It has session, entry, message, tool_use, and tool_use_file, with the stored
tables' columns, and blame and message_text over the same sessions; no other
table is present. Its rows key on the native id: id in session, session_id in
the rest. In its session table modified is the session file's modification time,
size is the file's size, and created and origin are NULL.

A stored session's origin ends in its native UUID, so
SELECT id FROM session WHERE origin LIKE '%/3fefde6c%' on the main store tells
whether a native session is already stored."#,
    },
    HelpTopic {
        topic: "message_text",
        short: "full-text message search and its query syntax",
        full: r#"message_text(query [, snippet_len]) searches the text of every stored session's
messages, or every session of the native source the query names. It returns (url,
session_id, line, type, subtype, score, snippet), one row per matching message,
best match first. url names the session across every source and store
(claude:<root>/<native id>, gage:<store>/<id>); session_id is the id the corpus
uses, so a row joins to message on (session_id, line). Matched terms in snippet
are wrapped in «guillemets»; snippet_len caps its length, default 200. LIMIT n reaches the search; without one the cap is 100
rows. A WHERE term on session_id, type, or subtype alone, as = or IN with
literal values, reaches the search too, so the cap counts admitted hits alone.
The search
reconciles its index first, so a message written since the last query is found.
message_text is absent from a scan-scoped context.

Message kinds: type is user, assistant, or system, and subtype narrows it.
Typed conversation is subtype text under type user or assistant. A tool call
is (assistant, tool_use) and its result is (user, tool_result), so
type = 'user' alone admits tool output. Model reasoning is
(assistant, thinking). Context the harness wrote into the conversation is
subtype meta. The kinds are the same for every driver. With no term on type or subtype the search covers
every kind, and tool calls and results are most messages. To search only what
the user and the assistant wrote, add WHERE subtype = 'text' or the field
term +subtype:text.

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
GROUP BY session_id ORDER BY hits DESC;

Conversation text only:
SELECT session_id, line, type, snippet FROM message_text('prepared plan')
WHERE subtype = 'text' LIMIT 20;"#,
    },
    HelpTopic {
        topic: "blame",
        short: "the session call that wrote each line of a file",
        full: r#"blame(file [, lines]) returns one row per contiguous line range and candidate call: (line, line_end, session_id,
session_lines, tool_use_id, tool_name, timestamp, rank, match). line_end is null
for a single line. It reads the stored sessions, or the sessions of the native
source the query names. file is a path; lines is optional and selects lines as a number (42)
or as text ('10-20,42'). A line may have several candidate calls; rank 1 is the
strongest, and match names the evidence: patch (the call's recorded patch added
the line), input (its input carries the line and the driver judged it wrote the
file), command (its input carries the line, write unjudged), or neighbor (both
adjacent lines are patch matches of this call). An unattributed line yields no
row. session_lines is the selection to read, from the human prompt before the
call through the call. Filter WHERE rank = 1 for one attribution per line.

SELECT line, line_end, session_id, session_lines, tool_name, match
FROM blame('/path/to/file.rs', '10-20') WHERE rank = 1;"#,
    },
    HelpTopic {
        topic: "json",
        short: "reading raw, input_raw, result_raw, and content_schema",
        full: r#"raw on entry and message, and input_raw and result_raw on tool_use, are JSON text in the driver's native shape. content_schema
(content_format, name, schema) describes that shape as a JSON Schema, one row per
content format and native field. Read it before reading into raw:
SELECT name, schema FROM content_schema WHERE content_format = 'claude-jsonl 1';

A path is valid for one content format. Scope rows through session.content_format
first; on rows of another format the path
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
  FROM message
  WHERE session_id = '<ID>' AND type = 'assistant'
  GROUP BY json_extract(raw, '$.message.id'));

In codex-rollout 1, a row's raw is {timestamp, type, payload}. entry.type is that
type (response_item, event_msg, turn_context, ...) and entry.subtype the
payload's type. The model calls its tools by writing a script to one exec tool,
so a tool_use message holds the script. A tool_use row is what a script ran: a
command (exec_command), an MCP call (mcp__<server>__<tool>), or a patch
(apply_patch). Its result_raw is the item Codex recorded for it, and input_raw
that item's input. content_schema has no rows for this format."#,
    },
    HelpTopic {
        topic: "summary",
        short: "the summary JSON fields and token totals",
        full: r#"The derived attributes of a session are typed columns on session: title, model, message_count, line_count, is_empty,
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
result message verbatim. An agent still running has null result columns, and
cost_usd is null for an agent whose harness reports no cost.

scan_log (scan_id, name, text) is one row per log a scan wrote, at most three:
name is out (what the scanners printed), err (Gage's status lines and task
failures), or records (the tracing records, one line each). Read a failed scan's
err first: SELECT text FROM scan_log WHERE scan_id = '<SCAN_ID>' AND name = 'err';"#,
    },
    HelpTopic {
        topic: "links",
        short: "the commit-bearing link tables",
        full: r#"note_target_link, issue_evidence_link, scan_note_link, scan_issue_link,
scan_dataset_link, dataset_session_link, dataset_attachment_link, and
share_session_link are the forms of the link tables that carry the commit of
each end (note_commit, issue_commit, session_commit, and so on). Use them only
when a specific commit matters; the short forms (note_target, issue_evidence,
share_session, and the rest) join on ids."#,
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
        full: r#"A scan-scoped query tool may also be constrained to specific sessions and, per
session, to a line range. entry, message, and tool_use then return only in-range
rows, even though the SQL carries no such predicate. Rows outside the scope do
not exist from the tool's point of view; do not infer their absence from the
corpus."#,
    },
];
