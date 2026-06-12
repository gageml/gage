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

The `target` col value is encoded using the following scheme:

- `session:<id>` - refers to an entire session - use table cols `session.id` and
  `note.session_id` to deference

- `session:<id>:<line>` - refers to a specific session line (e.g. a user or
  assistant message, etc.) - use table cols `note.session_id` and `note.line` to
  dereference

- `session<id>:<start>-<end>` - refers to a range of session lines inclusive of
  `<start>` and `<end>`

- `scan:<scan_id>` - refers to a scan run, which dereferences to a set of
  scanned sessions - you have no way to dereference this type of note to this
  list of sessions but you may find the note useful nonetheless

- `project:<project_path>` - refers to a session project

### `entry` table

To get a session line referenced by a note, use `entry` table `session_id` and
`line` cols. It may be helpful to extend the range of entry rows using a range
query over `line`. E.g. if a note target is `session:abc:123` and you're
interested in line `123` of session `abc` and also preceding and succeeding
messages, you might use `WHERE line BETWEEN 113 AND 133`.

As this is an open exercise, use your best judgement.

### `session` table

The `session` table provides summaries per session: `project`, `mtime`, `size`,
`title`, `model`, `message_count`, `input_tokens`, `output_tokens`. Use this
information as further neutral evidence in your inquiry.

### `config` table

Use the `config` to read project and user config.

### Other tables

`issue` contains issues that have been identified in previous investigations.

`issue_evidence` is a detail table for `issue` containing cited notes.
