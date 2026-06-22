---
description: Writing SQL for the Gage Query tool, including full-text search over Claude Code messages via the message_text TVF and note-anchored message context via the note_message_context TVF.
disable-model-invocation: false
user-invocable: false
---

Extends the `mcp__plugin_gage_gage__Query` tool help. See that tool's
description for the table catalog and column lists.

## Full-text search: `message_text(query [, snippet_len])`

Table-valued function. One call = one Tantivy search over indexed
message text. Returns `(session_id, line, type, subtype, score,
snippet)`. Rows are BM25-ordered. Matched terms in `snippet` are
wrapped in `«guillemets»`. `LIMIT n` is pushed through; without a
limit the default cap is 100.

### Query syntax (Tantivy)

- Plain terms: `refactor cache` (OR by default)
- Require all: `refactor AND cache`, or `+refactor +cache`
- Exclude: `refactor -cache`
- Phrase: `"prepared plan"`
- Field scoping: `type:assistant`, `subtype:tool_use`,
  `session_id:abc123`
- Combine: `+telemetry +type:assistant`

### Recipe: full-text hit + surrounding message

```sql
SELECT m.session_id, m.line, m.type, m.subtype, mt.snippet, m.text
FROM message_text('prepared plan') mt
JOIN message m USING (session_id, line)
ORDER BY mt.score DESC
LIMIT 20;
```

### Recipe: count hits, group by session

```sql
SELECT session_id, count(*) AS hits
FROM message_text('telemetry')
GROUP BY session_id
ORDER BY hits DESC;
```

## Note-anchored context: `note_message_context(note_id, before, after)`

Table-valued function. Returns the messages around the line a note
targets, with the same schema as the `message` table. `note_id` may be
a full id or a unique prefix; an ambiguous prefix raises a plan error.
The note must have a non-null `line` in `session_note` --- whole-session
notes return no rows.

The window is the note's anchor span (`[line, COALESCE(line_end,
line)]`, always included verbatim) plus `before` messages immediately
preceding it and `after` messages immediately following it.
`before`/`after` count *messages* (rows where `text IS NOT NULL`),
not raw line offsets --- non-message entries in the JSONL (summaries,
meta) do not consume from the count. Walks stop at session boundaries;
fewer rows than requested are returned in that case.

All three arguments must be SQL literals: a string literal for
`note_id` and integer literals for `before` and `after`. The TVF
resolves a single note per call --- it can't take a column reference
on the note id.

### Recipe: read the conversation around a note

```sql
SELECT line, type, subtype, text
FROM note_message_context('abc123', 3, 3)
ORDER BY line;
```

To scan many notes' context in one query, use a `ROW_NUMBER()` windowed
CTE over `message` joined to `session_note` --- the TVF is for the
single-note case.

## DataFusion limitations

- **Correlated scalar subqueries in the SELECT list are not implemented.** A
  query like `SELECT i.line, (SELECT m.text FROM message m WHERE m.line >
  i.line ORDER BY m.line LIMIT 1) FROM interrupts i` will fail with
  "Physical plan does not support logical expression ScalarSubquery". Use
  a `ROW_NUMBER()` windowed CTE instead.
