User interrupted a session

A user may interrupt a session any reason by pressing `Esc`. The presence of an
interrupt does not carry intrinsic meaning. Use
`note_message_context(note.id, 1, 1)` (one message prior and one message
following) for additional context.

**value** - `user` if from a user interrupt or `tool_use` if from user interrupt
for tool use.

**target** - session line
