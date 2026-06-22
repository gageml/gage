Counts of user interruptions in a session, broken out by what was
interrupted

**value** - JSON object:

- `user` - count of turns interrupted with the bare
  `[Request interrupted by user]` marker (the model was generating a
  response).
- `tool_use` - count of turns interrupted with
  `[Request interrupted by user for tool use]` (the user rejected a tool
  use prompt).

**target** - session

The note records that interruptions occurred and of which kind. It
carries no information about why the user interrupted, what was being
generated, or what tool was rejected. Zero values are written.
