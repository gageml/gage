+++
name = "NoteWrite"

[parameters.name]
type = "string"
required = true
description = "Note name, one of the allowed values the description lists"

[parameters.value]
type = "string"
required = true
description = "Note text"

[parameters.session_id]
type = "string"
required = false
description = "The session the note is about, by ID or unique prefix"

[parameters.session_lines]
type = "string"
required = false
description = "A line or range within the session, such as 12 or 12-20. Requires session_id"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to record a note: an observation about a session, or about the
work at hand when no session applies.

`name` says what kind of note this is and must be one of the allowed
values below; each value's doc says what that kind holds. Give
`session_id`, and `session_lines` when the note is about particular
lines, so the note is placed where it applies and later reads reach
it from the session.
