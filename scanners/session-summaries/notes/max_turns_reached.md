Number of `max_turns_reached` attachment entries in a session

**value** - integer count of attachment entries whose inner type is
`max_turns_reached`. Zero is written.

**target** - session

The harness writes a `max_turns_reached` attachment when the
configured per-turn limit stops the assistant mid-task. A nonzero count
means the session hit that limit at least once; the note does not
record what the limit was or what work was in progress when it
triggered.
