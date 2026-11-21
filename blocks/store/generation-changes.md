---
status: active
---

# Generation changes

Every Gage ref is under `refs/gage/<generation>/`.

The generation names the conventions a release needs before it can read an
object's type: the ref layout, the marker files of an object, the link file
convention, and the tombstone rule.

A release of Gage reads and writes one generation. It leaves refs of any other
generation alone.

A generation change is the last resort. The generation MUST change only for a
change to one of the conventions it names. A change to the content of one object
type MUST NOT change the generation.

A generation change requires a migration of every store, a new fetch
configuration for every remote, and new hooks on every Gage store used as a
remote.
