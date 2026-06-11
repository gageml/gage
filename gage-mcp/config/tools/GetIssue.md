+++
name = "GetIssue"

[parameters.issue_id]
type = "string"
required = true
description = "Issue ID (or unambiguous prefix)"

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Fetch the full detail for a single issue: description, originating
scanner, the notes linked to it as evidence, and its history — the log
of create, close, reopen, and comment events recorded against it.

Use after call to ListIssues to get more detail about a specific issue.

Use the issue detail for context and fix advice. If you complete the
issue or decide to skip the issue, use the CloseIssue tool.

---eof-678---
