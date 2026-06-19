---
description: Writing SQL for the Gage Query tool, including full-text search over Claude Code messages via the message_text TVF.
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

## Recipe: first event after a marker line

For "find the next `message` row after each marker line" (e.g. the next user
message after each `msg.interrupt` note), use a window function. A correlated
scalar subquery in the SELECT list will not work --- see the limitation below.

```sql
WITH markers AS (
  SELECT sn.session_id, sn.line AS marker_line
  FROM note n
  JOIN session_note sn ON sn.note_id = n.id
  WHERE n.name = 'msg.interrupt'
),
ranked AS (
  SELECT mk.session_id, mk.marker_line, m.line, m.text,
         ROW_NUMBER() OVER (
           PARTITION BY mk.session_id, mk.marker_line
           ORDER BY m.line ASC
         ) AS rn
  FROM markers mk
  JOIN message m
    ON m.session_id = mk.session_id
   AND m.line > mk.marker_line
   AND m.type = 'user'
)
SELECT session_id, marker_line, line, text
FROM ranked WHERE rn = 1;
```

For the previous event, swap `m.line > mk.marker_line` for `m.line <
mk.marker_line` and `ORDER BY m.line ASC` for `DESC`.

## DataFusion limitations

- **Correlated scalar subqueries in the SELECT list are not implemented.** A
  query like `SELECT i.line, (SELECT m.text FROM message m WHERE m.line >
  i.line ORDER BY m.line LIMIT 1) FROM interrupts i` will fail with
  "Physical plan does not support logical expression ScalarSubquery". Use a
  windowed CTE (see the recipe above) instead.
