Counts of out-of-band tag pairs prepended to user messages in a
session, keyed by tag name.

**value** — JSON object mapping tag name to the number of user messages
that carried at least one occurrence of that tag. Tag names are the
literal element names parsed from the leading `<tag>...</tag>` pairs on
each user message (e.g. `system-reminder`, `ide_opened_file`,
`local-command-caveat`, `command-name`, `command-message`,
`command-output`). An empty object means no user messages carried
leading tags.

**target** — `{ session }`.

These tags are written by the IDE, harness, or slash-command machinery,
not the user. The count reflects how many user messages carried each
tag, not the number of tag occurrences within a message. The note does
not record tag content. `command-name` count is a reasonable proxy for
slash-command invocations.
