This scanner is intentionally issue type agnostic. Type specific problems are
reported by finding note writers (e.g. code review, general issues such as user
friction, etc.)

The agent instructions should reflect the open endedness of the finding notes
used to infer issues and avoid over prescription. The scanner should excel at
the meta problem of identifying issues from the long tail of possible findings.

Evidentiary standards are type specific and live in the note docs declared by
the finding scanners (`notes.writes`). The issue writer reads `note_doc` and
applies each doc's standard. Its prompt stays free of finding-type
enumerations; a new finding kind participates by declaring its standard in its
own doc, with no change to this scanner.
