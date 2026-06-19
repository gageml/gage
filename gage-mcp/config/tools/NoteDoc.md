+++
name = "NoteDoc"

[parameters.name]
type = "string"
required = true
description = "Note name as declared in a scanner's `notes.writes`"

[annotations]
read_only_hint = true
idempotent_hint = true
+++

Return the docstring for a scanner note.

The docstring is whatever the scanner author wrote alongside the note's
entry in `notes.writes`. Scanners must declare every note they write, so
a note that the model encounters in query results (e.g. via the `notes`
table) will have a docstring here.

Notes are a flat global namespace across all scanners. If two scanners
declare the same note name, the last declaration wins (deterministic by
scanner+task name). Returns isError=true with "Not found" when the name
is not declared by any loaded scanner.

---eof-678---
