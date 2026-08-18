+++
name = "NoteWrite"

[parameters.name]
type = "string"
required = true
description = "Note name; must be one of the names this tool allows (error output lists them)"

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

`name` selects the kind of note being written. The allowed names are
task-defined; a wrong name returns an error listing the allowed names and
what each is for.

Provide `session_id` and optionally `session_line` to attach the note to a
specific session or line. Otherwise omit these inputs and the note attaches
to the active scan.

---eof-345---
