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
