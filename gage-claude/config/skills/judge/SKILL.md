---
description: Review available evidence and identify underlying issues.
disable-model-invocation: true
---

Review evidence from the `note` table using the `mcp__plugin_gage_gage__Query`
tool. User wants to find issues that can be resolved but does not know what they
might be. This is an open exercise. Your job is to assess what you find, conduct
further research using Gage tools, and finally make a judgement about underlying
issues. User will review your findings and discuss with you ways to resolve each
issue.

## Your main tool: `mcp__plugin_gage_gage__Query`

Use `Query` to read notes and session details. Everything in the session record
is available via `Query`. This is an optimized interface supporting your
investigation.

### `note` table

Use `note` table to read notes. Use the note `target` col to identify the
session and (optionally) session line number the note is referencing.

Note cols:

- `id` - note uuid
- `author` - who wrote the note; this will typically be a scanner
- `created` - when the note was created
- `modified` - when the note was last modified
- `target` - what the note applies to
- `name` - note name
- `value` - note value
- `metadata` - additional data associated with value

`name` is assigned by the author and is used consistently for the same value
type. It is only generally descriptive. Do your best to infer its meaning.

`value` is assigned by the author. It is a JSON encoded value and is either a
scalar or an object.

`metadata` is an optional set of additional named values encoded as a JSON
object.

`target` tells you what the note referenced. This is essential in your
investigation. It tells you where to look for the detail underlying a note.

`target` is advisory --- do not parse it. Use the prefix to pick the right link
table and join through it:

- `session:<id>` / `session:<id>:<line>` / `session:<id>:<start>-<end>` - refers
  to a session, optionally a specific line or line range. Join through the
  `session_note` link table: `JOIN session_note sn ON sn.note_id = n.id`, then
  `sn.session_id`, `sn.line`, `sn.line_end` give you what you need.

- `scan:<scan_id>` - refers to a scan run, which dereferences to a set of
  scanned sessions - you have no way to dereference this type of note to this
  list of sessions but you may find the note useful nonetheless

- `project:<project_path>` - refers to a session project

### `session_note`, `message`, and `entry` tables

To get a session line referenced by a note, join `note` to `session_note` on
`note_id`, then to `message` on `session_id` and `line`:

```sql
SELECT m.text
FROM note n
JOIN session_note sn ON sn.note_id = n.id
JOIN message m ON m.session_id = sn.session_id AND m.line = sn.line
WHERE n.name = '...'
```

To extend to surrounding messages, widen the `message` join predicate, e.g.
`AND m.line BETWEEN sn.line - 10 AND sn.line + 10`, or `AND m.line > sn.line`
for forward search, `AND m.line < sn.line` for backward.

The `message` table's `text` col contains the rendered message content. The
`entry` table provides the raw JSONL row per session line --- use it when
`message.text` and the other `message` cols don't have what you need.

As this is an open exercise, use your best judgement.

### `session` table

The `session` table provides summaries per session: `project`, `mtime`, `size`,
`title`, `model`, `message_count`, `input_tokens`, `output_tokens`. Use this
information as further neutral evidence in your inquiry.

### `config` table

Use the `config` to read project and user config.

### Other tables

`issue` and `issue_evidence` hold issues identified in previous investigations.
For this exercise no issues exist yet --- `IssueList` will return empty and
these tables are empty. Skip them; do not call `IssueList` and do not query
`issue` or `issue_evidence`.
