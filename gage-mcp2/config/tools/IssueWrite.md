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
description = "Note IDs that support this issue. Each becomes a linked evidence note."

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to write a new issue for a finding you have judged from the evidence.

Write one issue per distinct finding. Include the note IDs that support the
finding in `evidence` so the judgment links back to the evidence it rests on.
An issue reaches the sessions it concerns through the notes it cites, so cite
the notes rather than naming sessions.

The issue's name and initial status are fixed by the caller; you supply the
title, the description, and the evidence.
