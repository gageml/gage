+++
name = "IssueUpdate"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID or unique prefix"

[parameters.status]
type = "string"
required = true
description = "The new status: open or closed"

[parameters.status_reason]
type = "string"
required = false
description = "Why the issue is closed: completed, wontfix, or duplicate. Closing only; default completed"

[parameters.duplicate_of]
type = "string"
required = false
description = "The surviving issue's ID when status_reason is duplicate. Required then, not allowed otherwise"

[parameters.message]
type = "string"
required = false
description = "A message recorded with the status change"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to open or close an issue. The change is recorded as a status
event in the issue's history, with your message when given.

`status=open` promotes a pending issue, reopens a closed one, or
releases an assigned one. `status=closed` closes a pending, open, or
assigned issue, and `status_reason` says why: `completed` when the condition was fixed or acted on,
`wontfix` when it will not be addressed, `duplicate` when another
issue already describes the condition. A duplicate close names the
surviving issue in `duplicate_of`, so the link between the two is data rather
than prose; record any insight the survivor lacks on the survivor
with `IssueComment`.

An issue already in the requested status is refused. To comment
without changing status, use `IssueComment`.
