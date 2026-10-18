+++
name = "IssuesFree"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to free assigned issues whose session is gone. The tool takes no
input. It reads every assigned issue, asks whether the session working
it is still a live instance, and reopens the issues whose session is
gone so another session can take them up.

An issue held by a live session is left assigned. An issue whose
session is not live is freed at once. An issue whose session liveness
cannot be determined is freed once its assignment is more than a day
old. Every issue left assigned is named in the result with the reason
and the assignment's age; the user frees one by hand with "gage issue
open".
