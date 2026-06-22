Counts of attachment entries in a session, keyed by attachment subtype

**value** - JSON object mapping attachment subtype to count. Known subtypes
include `deferred_tools_delta`, `skill_listing`, `mcp_instructions_delta`, and
`max_turns_reached`. Attachment entries whose inner type is not recognized are
recorded under the key `"<unknown>"`. An empty object means no attachment
entries were recorded.

**target** - session

Attachment entries are out-of-band metadata the harness writes alongside the
conversation (tool registration deltas, skill listings, MCP instructions). The
note records their occurrence, not their content. Note that `max_turns_reached`
is also surfaced standalone as `session.max_turns_reached`.
