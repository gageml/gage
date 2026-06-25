+++
name = "CommentWrite"

[parameters.text]
type = "string"
required = true
description = "Comment text"

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

Use to record a free-form comment.

The comment is stored as a `comment` note. Provide `session_id` and optionally
`session_line` to attach the comment to a specific session or line if
applicable. Otherwise omit these inputs.
