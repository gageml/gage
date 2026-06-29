+++
name = "NoteWrite"

[parameters.type]
type = "string"
required = true
description = "Note type: `comment` or `finding`"

[parameters.value]
type = "string"
required = true
description = "Note text"

[parameters.session_id]
type = "string"
required = false
description = "Target session ID if applicable"

[parameters.session_line]
type = "integer"
required = false
description = "Target session line (1-indexed) if applicable (requires session_id)"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to record a note about the current scan or a session.

`type` selects the kind of note: `comment` for free-form remarks, `finding`
for an observation worth surfacing. The note name is `{type}.{short_id}`
where `short_id` is derived from the new note's ID.

Provide `session_id` and optionally `session_line` to attach the note to a
specific session or line. Otherwise omit these inputs and the note attaches
to the active scan.
