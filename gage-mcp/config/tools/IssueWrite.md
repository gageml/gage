+++
name = "IssueWrite"

[parameters.title]
type = "string"
required = true
description = "Short human-readable title"

[parameters.description]
type = "string"
required = false
description = "Markdown body describing the finding and the supporting reasoning"

[parameters.evidence]
type = "array"
items = { type = "string" }
required = false
description = "Note IDs that support this issue. Each becomes a linked evidence row."

[parameters.session_id]
type = "string"
required = false
description = "Target session ID if applicable. Sessions targeted by evidence notes are associated automatically."

[parameters.session_line]
type = "integer"
required = false
description = "Target session line (1-indexed) if applicable (requires session_id)"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to write a new issue for a finding you have judged from the evidence.

Write one issue per distinct finding. Include the note IDs that support the
finding in `evidence` so the judgment links back to the evidence it rests on.

Provide `session_id` and optionally `session_line` to target the issue at a
specific session or line directly, when there is no evidence note to cite. An
issue covering more than one session should cite evidence notes instead.

Issues are written as pending and reviewed by a later reconciliation step,
which opens them or resolves them against existing issues.

---eof-789---
