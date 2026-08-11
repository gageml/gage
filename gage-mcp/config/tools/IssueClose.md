+++
name = "IssueClose"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID (from list or detail)"

[parameters.reason]
type = "string"
required = true
description = "One of: completed, skipped, duplicate"

[parameters.message]
type = "string"
required = false
description = "Optional message recorded with the close event"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to mark an issue as closed.

You can close an issue with one of three reasons: completed, skipped, or
duplicate. If a fix was applied or some other action taken, use
completed. If the issue duplicates another issue that remains the issue
of record, use duplicate. Otherwise use skipped.

---eof-567---
