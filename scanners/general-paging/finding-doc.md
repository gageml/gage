Noteworthy finding in one page of a session

An observation of agent behavior or user friction, written by an agent that
read one contiguous page of a session record. The value is a description of
the observation. The target is the session. `lines` metadata, when present,
gives the session lines the observation refers to.

The writer sees a single page and skips problems that page shows being
diagnosed and fixed. It cannot see the rest of the session, so a reported
problem may be resolved elsewhere in the session; verify against the session
content before treating such a finding as unresolved. It has no visibility
across sessions.
