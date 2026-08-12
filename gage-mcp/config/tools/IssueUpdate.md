+++
name = "IssueUpdate"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID (from list or detail)"

[parameters.status]
type = "string"
required = false
description = "One of: open, closed, pending"

[parameters.status_reason]
type = "string"
required = false
description = "One of: completed, skipped, duplicate. Only meaningful when status = closed; ignored otherwise"

[parameters.message]
type = "string"
required = false
description = "Optional message recorded with the status event"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to update an issue. Currently only status is supported. Additional
fields (title, description, etc.) may be added later.

Set `status` to `open`, `closed`, or `pending`. When closing, use
`status_reason` to say why: `completed` if a fix was applied or action
taken, `skipped` if intentionally not addressed, `duplicate` if it
duplicates another issue. `status_reason` is ignored when `status` is
`open` or `pending`.

`message` is recorded on the status event for the log. To add a
comment without changing status, use `IssueComment` instead — a call
with no `status` is a no-op.

---eof-567---
