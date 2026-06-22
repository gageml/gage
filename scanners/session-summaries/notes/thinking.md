Counts of assistant thinking blocks in a session

**value** - JSON object:

- `total` - total number of assistant messages whose subtype is
  `thinking` (i.e. the message contains at least one `thinking` content
  block).
- `empty` - subset of `total` whose extracted thinking text is the
  empty string. Empty thinking is the signal that the harness recorded
  a thinking turn but did not include its content - typically because
  the `showThinkingSummaries` setting is disabled.

**target** - session

The note records how often the model thought and how often that
thinking was hidden. It does not record thinking content. Zero values
are written.
