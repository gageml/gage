+++
name = "IssueOpen"

[parameters.title]
type = "string"
required = true
description = "Short human-readable title"

[parameters.description]
type = "string"
required = false
description = "Markdown body describing the finding and the supporting reasoning"

[parameters.target]
type = "string"
required = false
description = "Optional target URI using the same scheme as note.target (e.g. session:<id>). Leave unset for a global issue."

[parameters.evidence]
type = "array"
items = { type = "string" }
required = false
description = "Note IDs that support this issue. Each becomes a linked evidence row."

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to open a new issue for a finding you have judged from the evidence.

Open one issue per distinct finding. Include the note IDs that support
the finding in `evidence` so the judgment links back to the evidence it
rests on.

---eof-789---
