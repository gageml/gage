Counts of messages in a session, keyed by `type.subtype`.

**value** — JSON object mapping `"<type>.<subtype>"` to message count.
Keys observed in practice:

- `assistant.text`
- `assistant.thinking`
- `assistant.tool_use`
- `user.text`
- `user.tool_result`
- `user.meta`

Combinations that do not occur are absent (not written as zero).

**target** — `{ session }`.

The breakdown distinguishes substantive user input (`user.text`) from
harness-injected meta entries (`user.meta`) and tool replies
(`user.tool_result`). It also distinguishes assistant turns that
produced visible text from those that produced only tool calls or
thinking. The `session` table's `message_count` column is the sum of
all keys here.
