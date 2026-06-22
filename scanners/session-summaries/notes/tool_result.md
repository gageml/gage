Counts of user `tool_result` blocks in a session, partitioned by
outcome

**value** - JSON object:

- `ok` - tool_result blocks whose `is_error` field is absent or false.
- `error` - tool_result blocks whose `is_error` field is true.

**target** - session

A tool_result is the harness's reply to one assistant `tool_use`; the
total should match `session.tool_use` totals modulo in-flight calls at
session end. The note does not break results down by tool - see
`session.tool_error` for per-tool error counts. Zero values are written.
