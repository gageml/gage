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
  session_type, driver, content_format, native_id, native_source,
  native_size)
- entry (session_id, line, uuid, type, subtype, raw) - one row per transcript
  line, `raw` its JSON
- message (session_id, line, uuid, type, subtype, text, attachments, ide_tags,
  raw) - conversation text; `type` is user, assistant, or system
- tool_use (session_id, id, name, line, uuid, timestamp, result_line,
  result_uuid, result_timestamp, output, is_error, input_raw, result_raw,
  files_changed) - one tool call with its result; `files_changed` is the
  paths written, null when unknown
- note (id, name, author, value, text, metadata, target, scan) - `name` is
  the note kind
- session_note (session_id, note_id, lines) - notes on a session, with the
  lines cited
- note_doc (note_name, doc, written_by) - what each note kind holds
- issue (id, name, title, description, status, status_reason, author,
  created, scan, key) - issues in scope; issue_event
  (issue_id, event_id, timestamp, author, event, from_status, to_status,
  reason, of, message); issue_evidence (issue_id, note_id); session_issue
  (session_id, issue_id)
- scan (id, started, stopped, canceled, tasks, completed, failed, dataset);
  scan_session (scan_id, session_num, session_id); scan_note (scan_id,
  note_id, carried); scan_issue (scan_id, issue_id)
- dataset (id); dataset_session (dataset_id, session_num, session_id)
- attachment (id, name, root, includes, excludes, file_count, size);
  attachment_file (attachment_id, path, size, text)

Functions: issue_report('<id>') renders one issue as markdown;
related_issue('<id>' [, n]) lists the n most similar issues, best first.

Read a session by `session_id` ordered by `line`, taking
`substr(text, 1, 800)` over full `text` or `raw` on wide rows.
