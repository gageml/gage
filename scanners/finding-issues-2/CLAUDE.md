This scanner is intentionally issue type agnostic. Type specific problems are
reported by finding note writers (e.g. code review, general issues such as user
friction, etc.)

The agent instructions should reflect the open endedness of the finding notes
used to infer issues and avoid over prescription. The scanner should excel at
the meta problem of identifying issues from the long tail of possible findings.

Note docs (declared by finding scanners under `notes.writes`) describe the
notes as they are and carry no issue-writing policy. Evidentiary standards live
in this scanner's prompt, stated generically so confidence follows from what a
doc says about its notes. A new finding kind participates by declaring a
descriptive doc, with no change to this scanner.
