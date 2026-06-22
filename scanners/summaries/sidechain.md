Number of sidechain entries in a session.

**value** — integer count of entries whose raw JSON has
`isSidechain: true`. Zero is written.

**target** — `{ session }`.

Sidechain entries are messages produced by subagent (Task tool)
invocations rather than the main conversation. A nonzero count
indicates the session delegated work to one or more subagents. The
count is over entries, not over distinct subagent invocations.
