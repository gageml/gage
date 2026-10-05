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

Run SQL (DataFusion dialect) over this scan: its sessions, or the one session
in scope, and the notes and issues it holds. Nothing outside the scan is
visible. Results are YAML rows, capped per page; a truncated page says how to
continue.

Tables:

- session (id, project, title, model, message_count, line_count, is_empty,
  session_type, driver, native_id, native_source, native_size)
- entry (session_id, line, uuid, type, subtype, raw) - one row per transcript
  line; `raw` is the line's JSON
- message (session_id, line, uuid, type, subtype, text, attachments, ide_tags,
  raw) - conversation text per line; `type` is user, assistant, or system
- note (id, name, author, value, text, metadata, target, scan) - the scan's
  notes, written by it or carried from earlier scans; `name` is the note kind
- session_note (session_id, note_id, lines) - notes placed on a session with
  the line or range cited; join note to session or message here
- issue (id, name, title, description, status, status_reason, author,
  evidence_count, created, scan, key) - the issues the scan wrote; issue_event
  (issue_id, event_id, timestamp, author, event, from_status, to_status,
  reason, message); issue_evidence (issue_id, note_id); session_issue
  (session_id, issue_id)
- scan (id, started, stopped, canceled, tasks, completed, failed, skipped,
  dataset); scan_session (scan_id, session_num, session_id); scan_note
  (scan_id, note_id, carried); scan_issue (scan_id, issue_id)
- dataset (id); dataset_session (dataset_id, session_num, session_id)
- attachment (id, name, root, includes, excludes, file_count, size);
  attachment_file (attachment_id, key, size, content) - each file's bytes

Filter by `session_id` and order by `line` when reading a session. Select
`substr(text, 1, 800)` rather than full `text` or `raw` on wide rows. When the
scope is one session or a line range, entry and message already hold only
those rows.
