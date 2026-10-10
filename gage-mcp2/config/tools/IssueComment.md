+++
name = "IssueComment"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID or unique prefix"

[parameters.comment]
type = "string"
required = true
description = "Comment text"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to add a comment to an issue without changing its status. The
comment is recorded as a comment event in the issue's history with
you as its author. Use it for observations, follow-up findings, or
the insight a duplicate carried that its surviving issue lacked.
