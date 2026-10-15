+++
name = "Query"

[parameters.sql]
type = "string"
required = true
description = "SQL (DataFusion dialect) to run against the session data in scope."

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Run SQL (DataFusion dialect) over the sessions in scope and the notes and
issues they hold. Results are YAML rows, capped per page; a cut page says how
to go on.

Tables:

- session (id, project, title, model, message_count, line_count, is_empty,
  input_tokens, output_tokens, cached_tokens, cost_usd, created, modified,
  session_type, driver, content_format, native_id, native_source,
  native_size, native_mtime) - token columns are session totals: every prompt
  token sent, every token generated, and the cached part of input; cost_usd
  is the harness's own figure, null when it reports none
- entry (session_id, line, uuid, type, subtype, timestamp, raw) - one row per
  transcript line, `raw` its JSON
- message (session_id, line, uuid, type, subtype, timestamp, text,
  attachments, ide_tags, raw) - conversation text; `type` is user, assistant,
  or system
- tool_use (session_id, id, name, line, uuid, timestamp, result_line,
  result_uuid, result_timestamp, output, is_error, input_raw, result_raw,
  files_changed) - one tool call with its result; `files_changed` is the
  paths written, null when unknown
- note (id, name, author, value, text, metadata, target, scan, work_id,
  created, modified) - `name` is the note kind
- session_note (session_id, note_id, lines) - notes on a session, with the
  lines cited
- note_doc (note_name, doc, written_by) - what each note kind holds
- issue (id, name, title, description, status, status_reason, author,
  created, modified, scan, key) - issues in scope; issue_event
  (issue_id, event_id, timestamp, author, event, from_status, to_status,
  reason, duplicate_of, message); issue_evidence (issue_id, note_id);
  session_issue (session_id, issue_id)
- scan (id, runtime, started, stopped, canceled, tasks, completed, failed,
  dataset, created, modified); scan_session (scan_id, session_num,
  session_id); scan_note (scan_id, note_id, carried); scan_issue (scan_id,
  issue_id)
- dataset (id, origin, created, modified); dataset_session (dataset_id,
  session_num, session_id)
- attachment (id, name, key, targets, file_count, size, digest, created,
  modified); attachment_file (attachment_id, path, size, text)

Functions: issue_report('<id>') renders one issue as markdown;
related_issue('<id>' [, min_score]) lists the issues similar enough to be duplicate candidates, best first;
age(ts) renders time since a timestamp, as in 3h ago.
blame('<file>' [, lines]) and native_blame('<source>', '<file>' [, lines])
give one row per file line with the call that wrote it: (line, text,
session_id, lines, tool_use_id, tool_name, timestamp); `lines` is the session
lines to read, from the prompt before the call through it; session_id is null
when no call's input carries the line. lines is a number or '10-20,42'.

`raw`, `input_raw`, and `result_raw` are JSON text in the driver's native
shape. content_schema (content_format, name, schema) holds each shape as
a JSON Schema: `name` is entry, tool_use_input, or tool_use_result. Scope rows
to one `content_format` (from `session`) before reading, with
json_extract(raw, '$.message.usage.output_tokens') or the -> and ->>
operators; CAST the text for arithmetic. Do not cast to json or jsonb.

Native source functions read a driver source directly, including sessions
not in the store: native_session('<source>'), native_message('<source>'),
native_entry('<source>'), native_tool_use('<source>'). The argument is
required and is a source URL: '' names the default source, 'claude:' is the
default Claude Code source, and 'claude:/path' names a projects directory.
native_session returns (id, project, title, model, message_count, is_empty,
input_tokens, output_tokens, cached_tokens, cost_usd, content_format, mtime,
size); `id` is the native UUID, and there is no line_count, created, or
modified. The other three return the stored tables' columns, with the native
UUID as `session_id`. Use these to read a session by native id before it
is added to the store.

Read a session by `session_id` ordered by `line`, taking
`substr(text, 1, 800)` over full `text` or `raw` on wide rows.
