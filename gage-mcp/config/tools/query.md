+++
name = "query"

[parameters.sql]
type = "string"
description = "SQL (DataFusion dialect) to run against the session data in scope. The result is written under a cursor and page 1 is returned. Give sql or cursor, not both."

[parameters.cursor]
type = "string"
description = "Cursor of a result an earlier call returned, to read one of its pages. A result expires one hour after it was written."

[parameters.page]
type = "integer"
description = "Page of the cursor's result to return, starting at 1 (default 1). Goes with cursor."

[parameters.source]
type = "string"
description = "Run sql over this source instead of the main store, read only: gage:<path> is the Gage store at <path>; a native source is a driver's scheme and a path, as in claude:<path> for the Claude Code sessions under <path>/projects, or claude: for those in the default location. Goes with sql."

[parameters.publish]
type = "boolean"
description = "Run sql over pending publish sessions instead: sessions staged by gage publish and not yet applied. session, entry, message, tool_use, tool_use_file, and session_change hold them and nothing else; no other table is present. Goes with sql."

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Run SQL over the sessions in scope and the notes and issues they hold. Results
are YAML pages naming their cursor and next page.

Discover the schema through SQL. information_schema.tables lists the tables;
information_schema.columns gives a table's columns and types;
information_schema.routines lists the scalar and table functions. A function's
result columns come from DESCRIBE SELECT * FROM <func>(<args>).

The `help` table holds the usage notes introspection cannot: the table
functions' call signatures, the column and table semantics, the full-text query
syntax, and the dialect limits. Read a topic with
`SELECT full FROM help WHERE topic = '<topic>'`. The topics:

source: reading another store or a native source in place of the main store
message_text: full-text message search and its query syntax
blame: the session call that wrote each line of a file
json: reading raw, input_raw, result_raw, and content_schema
summary: the summary JSON fields and token totals
tool_use_file: the write, read, and cite file-touch rows
notes: the note, session_note, and note_doc tables
issues: issue_report, related_issue, and the issue tables
scans: the scan cohort, task, agent, and log tables
links: the commit-bearing link tables
system_cols: the row-addressing system columns
limitations: DataFusion dialect limits to plan around
scopes: what a scan-scoped context restricts

The named tables are session, entry, message, tool_use, tool_use_file,
session_change, note, note_doc, issue, issue_event, issue_evidence, scan and its
link and log tables, dataset, attachment, and content_schema. session is keyed
by `id`; every other table names its session `session_id`, and rows within a
session are ordered by `line`.

session carries the derived attributes as typed columns (`title`, `model`,
`message_count`, `line_count`, `is_empty`, the token totals, `cost_usd`), the
same values with the driver's extra members in a `summary` JSON, and `origin`
the source URL.

message is one row per message. `type` is who produced it: user, assistant, or
system. `subtype` is what it holds: text, thinking, tool_use, or tool_result,
and for system messages kinds such as api_error or compact_boundary. Typed
conversation is subtype text under type user or assistant; a tool result is
(user, tool_result), so type = 'user' alone admits tool output.

tool_use is one row per tool call, joined to its result: `name`, `output`, and
`is_error`, true when the call failed.

entry and message carry `raw`, tool_use carries `input_raw` and `result_raw`,
each JSON in the driver's native shape.

Read a session's messages from message by `session_id` ordered by `line`,
taking `substr(text, 1, 800)` over full `text` or `raw` on wide rows.
