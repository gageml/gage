Code quality finding in a session

A defect in the project's code or other artifacts, observed in the code a
session wrote or changed: rule violations, best practice violations, poor stack
alignment, undetected bugs. The value is a description of the defect that's
concrete enough to check against current project content. The writer does not
perform that check. The note names the violated project rule. The target is the
session. `lines` metadata, when present, gives the session lines the defect
refers to.

The writer sees a single session and skips defects the session itself diagnosed
and fixed. It has no visibility across sessions.
