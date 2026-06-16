+++
name = "Query"

[parameters.sql]
type = "string"
required = true
description = "SQL query to execute against Gage session data."

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Execute SQL (DataFusion dialect) against Claude Code sessions, Gage
notes, and Gage issues.

Tables:

- session (id, project, path, mtime, size, title, model,
  message_count, input_tokens, output_tokens,
  cache_read_input_tokens, cache_creation_input_tokens, is_empty) -
  list of available sessions

- entry (session_id, line, uuid, type, timestamp, raw) - raw JSON per
  line per session

- message (session_id, line, uuid, type, subtype, text, timestamp,
  attachments, ide_tags, raw) - conversation text (user and
  assistant) per session

- note (id, author, created, modified, target, name, value, metadata,
  explanation) - notes written by scanners; `name` is the scanner
  output kind, `value` its payload, `explanation` the optional
  narrative

- issue (id, name, title, description, status, closed_reason,
  created, modified, author) - issues raised from notes

- issue_evidence (issue_id, note_id, name, timestamp, digest) - link
  table from issues to the notes that support them

TVF for full-text search over message text:

- message_text(query [, snippet_len]) -> (session_id, line, type,
  subtype, score, snippet) - Tantivy query string; BM25-ordered. See
  the `query` skill for syntax and recipes.

Hints:

- Users often refer to sessions using their prefix ID
- `message.text` is convenient for message text content in one value
- `entry.raw` is same as reading session JSONL at `line`
- Join `issue` to `note` through `issue_evidence` to inspect the
  evidence behind an issue

---eof-123---
