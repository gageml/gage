+++
name = "Query"

[parameters.sql]
type = "string"
description = "SQL (DataFusion dialect) to run against the session data in scope. The result is written under a cursor and page 1 is returned. Give sql or cursor, not both."

[parameters.cursor]
type = "string"
description = "Cursor of a result an earlier call returned, to read one of its pages. A result expires one hour after it was written."

[parameters.page]
type = "integer"
description = "Page of the cursor's result to return, starting at 1 (default 1). Goes with cursor."

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

native: session functions that read a source directly, for sessions not in the store
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
link and log tables, dataset, attachment, and content_schema. session carries
`title` and a `summary` JSON (model, counts, token totals, cost_usd), and
`origin` the source URL; entry and message carry `raw`, tool_use carries
`input_raw` and `result_raw`, each JSON in the driver's native shape.

Read a session by `session_id` ordered by `line`, taking `substr(text, 1, 800)`
over full `text` or `raw` on wide rows.
