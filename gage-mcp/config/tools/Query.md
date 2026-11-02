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
description = "Run sql over pending publish sessions instead: sessions staged by gage publish and not yet applied. session, entry, message, tool_use, and changes hold them and nothing else; no other table is present. Goes with sql."

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Run SQL over the sessions in scope and the notes and issues they hold. Results
are YAML pages naming their cursor and next page.

Discover the schema through SQL. information_schema.tables lists the tables;
information_schema.columns gives a table's columns and types;
information_schema.routines lists the scalar and table functions by name. A
function's result columns are read with DESCRIBE SELECT * FROM <func>(<args>),
the only way to see the native functions' driver-dependent shape. The tables and
their less obvious columns:

- session - `summary` is the driver's JSON: title, model, message_count,
  line_count, is_empty, token totals, and cost_usd when reported; `origin` is
  the session's source URL
- entry - one row per transcript line, `raw` its JSON
- message - conversation text; `type` is user, assistant, or system
- tool_use - one call with its result; `files_changed` is the paths written,
  null when unknown
- changes - changes a publish made to its original; `kind` is line, value, text,
  attr, or title
- note - `name` is the note kind; note_doc (note_name, doc) says what each kind
  holds; session_note (session_id, note_id, lines) cites a note's lines
- issue, issue_event, issue_evidence (issue_id, note_id), session_issue
- scan, scan_log (name is out, err, or records), scan_session, scan_note,
  scan_issue, scan_task, scan_task_agent
- dataset, dataset_session; attachment, attachment_file (path, size, text)
- content_schema (content_format, name, schema) - each native content shape as a
  JSON Schema; `name` is entry, tool_use_input, or tool_use_result

Functions, with their result columns read via DESCRIBE: issue_report('<id>')
renders one issue as markdown; related_issue('<id>' [, min_score]) lists likely
duplicates. blame('<file>' [, lines]) and native_blame('<source>', '<file>' [,
lines]) give the session call that wrote each file line; lines is a number or
'10-20,42'. message_text('<query>' [, snippet_len]) and
native_message_text('<source>', '<query>' [, snippet_len]) search message text,
best match first; `+a +b` requires both, `-a` excludes, "a b" is a phrase,
`type:assistant` restricts; LIMIT caps the hits, default 100.

`raw`, `input_raw`, and `result_raw` are JSON text in the driver's native shape.
Scope rows to one `content_format` (from `session`) before reading with
json_extract(raw, '$.message.usage.output_tokens') or -> and ->>; CAST the text
for arithmetic, never to json or jsonb.

Native source functions read a driver source directly, including sessions not in
the store: native_session, native_message, native_entry, and native_tool_use,
each taking '<source>': '' is the default source, 'claude:' the default Claude
Code source, 'claude:/path' a projects directory.

Read a session by `session_id` ordered by `line`, taking `substr(text, 1, 800)`
over full `text` or `raw` on wide rows.
