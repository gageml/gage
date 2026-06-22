Counts of assistant `tool_use` blocks in a session, keyed by tool name

**value** - JSON object mapping tool name to the number of `tool_use`
blocks the assistant emitted for that tool. Tools never invoked do not
appear as zero entries; an empty object means the assistant called no
tools.

**target** - session

The note records what tools were called and how often. It does not
record arguments, whether the call succeeded, or whether the result was
acted on. Pair with `session.tool_result` / `session.tool_error` for
outcomes.
