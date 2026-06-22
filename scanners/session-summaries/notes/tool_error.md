Counts of failed tool invocations in a session, keyed by tool name

**value** - JSON object mapping tool name to the number of
`tool_result` blocks with `is_error: true` attributable to that tool.
Tool name is resolved by matching `tool_use_id` back to the originating
assistant `tool_use` block. Errors whose originating call cannot be
resolved are recorded under the key `"<unknown>"`. An empty object means
no tool errors were recorded.

**target** - session

The note records that tool calls failed and which tools failed. It does
not record error messages, retry counts, or whether the model recovered.
A high count for one tool does not by itself indicate a tool defect:
model misuse and harness rejection both produce `is_error: true`
results.
