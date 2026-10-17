+++
name = "IssuesFree"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to free assigned issues whose session is no longer running. The
tool takes no input. It reads every assigned issue, asks whether the
session working it is still a live instance, and reopens the issues
whose session is gone so another session can take them up.

An issue held by a live session is left assigned. An issue whose
session liveness cannot be determined is left assigned and named in
the result, so nothing is freed on an inconclusive check.
