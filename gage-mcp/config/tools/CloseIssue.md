+++
name = "CloseIssue"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID (from list or detail)"

[parameters.reason]
type = "string"
required = true
description = "One of: completed, skipped"

[parameters.message]
type = "string"
required = false
description = "Optional message recorded with the close event"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to mark an issue as closed.

You can close an issue with one of two reasons: completed or skipped. If
a fix was applied or some other action take, use completed, otherwise
use skipped.

---eof-567---
