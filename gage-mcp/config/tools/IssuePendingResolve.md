+++
name = "IssuePendingResolve"

[parameters.resolutions]
type = "array"
items = { type = "object" }
required = true
description = "Dispositions, one per pending issue (see tool description for the object fields)"

[annotations]
read_only_hint = false
idempotent_hint = false
+++

Use to apply a pending issue resolution plan: every pending issue is
either promoted to open (it is novel) or closed as a duplicate of an
issue Gage already has.

Each resolution object has these fields:

- `issue` (string, required) - pending issue ID
- `action` (string, required) - `open` promotes the issue; `duplicate`
  closes it against another issue
- `of` (string) - the surviving issue a duplicate closes against.
  Required for `duplicate`.
- `comment` (string, optional) - comment added to the surviving issue,
  carrying insight the survivor lacks
- `reopen` (bool, optional) - when the surviving issue is closed,
  reopen it (the condition recurred). Without `reopen` a closed
  survivor stays closed (the report was already resolved).

The plan is validated as a whole before anything is written, and
applied in one transaction. A `duplicate` target must be an open
issue, a closed issue, or a pending issue promoted in the same plan.
Chains are rejected: if A closes against B and B also closes as a
duplicate, resolve A against B's final target instead.

A call need not dispose every pending issue; the result reports how
many pending issues remain.

---eof-456---
