---
description:
  Instructions for using mcp__plugin_gage_gage__Query (SQL access to Claude code
  session content, Gage notes, and issues)
disable-model-invocation: false
user-invocable: false
---

Extends the `mcp__plugin_gage_gage__Query` tool help. See that tool's
description for the table catalog and column lists.

## Sessions not in the store: the native source functions

The `session`, `entry`, `message`, and `tool_use` tables hold stored sessions
only. A session named by its native UUID (the Claude Code transcript id) may
not be stored. `native_session(source)`, `native_message(source)`,
`native_entry(source)`, and `native_tool_use(source)` read the source
directly. `'claude:'` is the default Claude Code source; `'claude:/path'`
names a projects directory. The native tables use the native UUID: `id` in
`native_session`, `session_id` in the other three.

```sql
SELECT id, project, message_count FROM native_session('claude:')
WHERE id LIKE '3fefde6c%';

SELECT line, type, substr(text, 1, 800) AS text
FROM native_message('claude:')
WHERE session_id = '3fefde6c-69be-4a8d-b210-c394c5d23725'
ORDER BY line;
```

A stored session's `native_id` is its native UUID, so
`SELECT id FROM session WHERE native_id LIKE '3fefde6c%'` tells whether a
native session is stored.

## Scan link tables: `scan_session`, `scan_note`, `scan_issue`

`scan_session (scan_id, session_num, session_id)` links a scan to the
sessions it covered, its cohort, in dataset member order. A scan with zero
notes still has its cohort rows here, so this is also the existence check
for a scan id.

`scan_note (scan_id, note_id, carried)` links a scan to the notes in its
visible set. `carried` is false when the scan wrote the note and true when
the note predates the scan and was carried forward. To select exactly the
notes a scan produced:

```sql
SELECT n.*
FROM note n
JOIN scan_note sc ON sc.note_id = n.id
WHERE sc.scan_id = '<SCAN_ID>' AND NOT sc.carried;
```

`scan_issue (scan_id, issue_id)` links a scan to the issues it wrote.

## Scan tasks: `scan_task`, `scan_task_agent`

`scan_task (scan_id, num, scanner, task, status, started, stopped,
worked_ms, selected)` is one row per task of a scan. `num` is the task's
position in the scan's plan, the order the runner dispatched. `status` is
`pending`, `started`, `completed`, `failed`, or `canceled`.

`scan_task_agent (scan_id, scanner, task, session_id, native_id, exit_code,
is_error, stop_reason, turns, duration_ms, cost_usd, result)` is one row per
agent a task ran. `session_id` is the agent's stored session, `native_id`
its native UUID, and `result` the harness's result message verbatim. An
agent still running has its `native_id` and null result columns.

## Notes

`note (id, name, target, author, value, text, metadata, created, modified,
scan, work_id)`. `value` is the note's JSON text; `text` is the plain-text
form alone and is null for a structured note. `target` names what the note
is on: `session:<id>`, `session:<id>#<lines>`, or `attachment:<id>`. `scan` is the scan the
note was written during. Each `name` has a row in `note_doc (note_name, doc,
written_by)` describing what notes of that name hold.

`session_note (session_id, note_id, lines)` is the session-targeted notes.
`lines` is the cited line ranges as text, for example `831-838` or
`211-282,317`, null when the note cites no lines.

## Commit-bearing link tables

`note_target_link`, `issue_evidence_link`, `scan_note_link`,
`scan_issue_link`, `scan_dataset_link`, `dataset_session_link`, and
`dataset_attachment_link` are the forms of the link tables that carry the
commit of each end (`note_commit`, `issue_commit`, `session_commit`, and so
on). Use them only when a specific commit matters; the short forms above
join on ids.

## System columns

A context that serves a program also exposes system columns: `id_display`,
`id_prefix`, `locator`, `commit`, and the `*_commit` columns. They address a
row rather than describe it. Leave them out of a listing unless a commit is
the point.

## DataFusion limitations

- **Correlated scalar subqueries in the SELECT list are not implemented.** A
  query like
  `SELECT i.line, (SELECT m.text FROM message m WHERE m.line > i.line ORDER BY m.line LIMIT 1) FROM interrupts i`
  will fail with "Physical plan does not support logical expression
  ScalarSubquery". Use a `ROW_NUMBER()` windowed CTE instead.

- **Two or more correlated aggregate subqueries in one SELECT list fail.** A
  query with both `(SELECT count(*) FROM scan_session x WHERE x.scan_id = s.id)`
  and `(SELECT count(*) FROM scan_note y WHERE y.scan_id = s.id)` fails with
  "Ambiguous reference to unqualified field". One such subquery works. Join a
  `(SELECT scan_id, count(*) ... GROUP BY scan_id)` derived table per count
  instead.

- **No `any_value` aggregate.** To carry a representative value through a
  `GROUP BY` (e.g. a session title), add the column to the `GROUP BY` or use
  `min(...)` / `max(...)`.

## Scoped contexts

A scan-scoped Query tool may also be constrained to specific sessions and,
per session, to a line range. `entry`, `message`, and `tool_use` then return
only in-range rows, even though the SQL carries no such predicate. Rows
outside the scope do not exist from the tool's point of view; do not infer
their absence from the corpus.
