---
description:
  Instructions for using mcp__plugin_gage_gage__Query (SQL access to Claude code
  session content, Gage notes, and issues)
disable-model-invocation: false
user-invocable: false
---

Extends the `mcp__plugin_gage_gage__Query` tool help. See that tool's
description for the table catalog and column lists.

## Scan link tables: `scan_session`, `scan_note`, `scan_issue`

`scan_session (scan_id, session_id, metadata)` links a scan to the sessions it
covered — its cohort, recorded at selection time. A scan with zero notes still
has its cohort rows here, so this is also the existence check for a scan id.

`scan_note (scan_id, note_id, role)` links a scan to the notes in its visible
set. `role` is `wrote` (the scan wrote the note's current value; a replace
relinks the replacing scan) or `carried` (the note predates the scan and was
carried forward). To select exactly the notes a scan produced, join on
`scan_note` with `role = 'wrote'`:

```sql
SELECT n.*
FROM note n
JOIN scan_note sc ON sc.note_id = n.id
WHERE sc.scan_id = '<SCAN_ID>' AND sc.role = 'wrote';
```

`scan_issue (scan_id, issue_id, role)` links a scan to the issues in its visible
set. `role` is `wrote` (the scan created the issue) or `carried` (the issue
predates the scan, which adopted it — adding evidence, linking a session, or
reopening it). An issue has at most one `wrote` link.

## DataFusion limitations

- **Correlated scalar subqueries in the SELECT list are not implemented.** A
  query like
  `SELECT i.line, (SELECT m.text FROM message m WHERE m.line > i.line ORDER BY m.line LIMIT 1) FROM interrupts i`
  will fail with "Physical plan does not support logical expression
  ScalarSubquery". Use a `ROW_NUMBER()` windowed CTE instead.

- **No `any_value` aggregate.** To carry a representative value through a
  `GROUP BY` (e.g. a session title), add the column to the `GROUP BY` or use
  `min(...)` / `max(...)`.

## Notes

Read notes `note` (id, name, target, author, value, metadata, created,
modified). Each note name has a corresponding entry in `note_doc` (note_name,
doc, written_by) that provides documentation for the note.

## Scoped contexts

A scan-scoped Query tool may also be constrained to specific sessions and, per
session, to a line range. `entry` and `message` then return only in-range rows,
even though the SQL carries no such predicate. Rows outside the scope do not
exist from the tool's point of view; do not infer their absence from the corpus.
