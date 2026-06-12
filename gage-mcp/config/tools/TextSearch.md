+++
name = "TextSearch"

[parameters.query]
type = "string"
required = true
description = "Tantivy query syntax. Plain terms ('refactor cache'), phrases ('\"prepared plan\"'), boolean operators (foo AND bar, +foo -bar), and field scoping (session_id:abc123, type:assistant). Multiple terms default to OR; combine with AND or + to require all."

[parameters.limit]
type = "integer"
required = false
description = "Maximum number of hits to return. Default 20. Hits are ordered by BM25 score, descending."

[parameters.snippet_len]
type = "integer"
required = false
description = "Approximate maximum length (in characters) of the snippet excerpt. Matched terms are wrapped in «guillemets»."

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Full-text search over Claude Code message content. Returns multi-document YAML
with `session_id`, `line`, `type`, `subtype`, `score`, and `snippet` (BM25
excerpt with matched terms in «guillemets»). `type` is `user`, `assistant`,
`summary`, or `attachment`; `subtype` qualifies it (`text`, `tool_use`,
`tool_result`, `thinking`, `meta`, …) and may be null.

Use `Query` to fetch surrounding message context once a hit looks interesting:
`SELECT * FROM message WHERE session_id = '...' AND line = N`.

---eof-789---
