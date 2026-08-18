+++
name = "IssueComment"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID (from list or detail)"

[parameters.comment]
type = "string"
required = true
description = "Comment text to record against the issue"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to add a comment to an issue.

The comment is logged as a `comment` event on the issue and bumps the
issue's last-activity timestamp. Use this to record observations,
follow-up findings, or discussion against an existing issue without
changing its status.

---eof-234---
