Number of conversation compaction events in a session

**value** - integer count of entries the harness writes when it
compacts prior conversation context into a summary (entry types such
as `compact-summary` / `summary`). Zero is written.

**target** - session

Compaction occurs when the conversation approaches the context window
and the harness replaces earlier turns with a generated summary. A
nonzero count means the session ran long enough to trigger this at
least once. The note does not record what was summarized or how much
was elided.
