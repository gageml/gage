# summaries

NOTE: This scanner is slated for removal, to be replaced by narrowly focused
scanners.

Per-session aggregate notes. Each task walks the session record and writes one
or more notes whose `value` is the signal in its natural shape (object when
multi-dimensional, scalar when single-dimensional). Zero values are written.
`target` is always `{ session }`.

Intended as non-biasing input to downstream issue analysis: the notes report
what happened and how often, not what it means.

## Notes

Each entry gives the note's `value` shape and what a reader can plausibly do
with it. See the note's own `.md` file for the full docstring.

`session.ide_tags` --- `{ "<tag>": n, ... }`. Counts of out-of-band tag pairs
prepended to user messages. `command-name` count approximates slash-command
invocations. Other keys are harness/IDE chatter.

`session.model` --- `{ "<model_id>": n, ... }`. Mid-session model switches and
per-model share. Lets a reader attribute issues to a specific model when
multiple were used.

`session.duration_ms` --- integer. Wall-clock span from first to last message
entry, including idle. Weak alone; gains meaning when divided by `message_count`
(density) or compared across sessions.

`session.sidechain` --- integer. Count of `isSidechain: true` entries. Nonzero
explains gaps where the main stream looks idle while subagent (Task tool) work
ran.

`session.compact` --- integer. Count of conversation compaction events. Nonzero
explains "model forgot earlier discussion" symptoms: those references were
elided.

`session.api_error` --- integer. Count of harness-recorded API failures.
Distinguishes infrastructure failures from model-behavior failures.

`session.tool_use` --- `{ "<tool>": n, ... }`. Per-tool call counts.
Characterizes how the model worked (Read-heavy, Bash-heavy, Write-heavy).
Surfaces tool spam.

`session.tool_result` --- `{ ok, error }`. Bulk success/error counts of
`tool_result` blocks. Largely redundant with `tool_use` + `tool_error`.

`session.tool_error` --- `{ "<tool>": n, ... }`. Per-tool failure counts
(`tool_result.is_error == true`, joined back via `tool_use_id`). Surfaces a tool
failing repeatedly vs scattered one-offs.

`session.attachment` --- `{ "<subtype>": n, ... }`. Counts of harness
out-of-band attachment entries by subtype. Mostly harness setup chatter;
`max_turns_reached` is the one issue-relevant key and is also a standalone note.

`session.max_turns_reached` --- integer. Count of `max_turns_reached` attachment
entries. Nonzero means the harness stopped the assistant mid-task; direct cause
of truncated-work complaints.

## Tasks

- `interrupt` --- writes `session.interrupt`
- `message_count` --- writes `session.message_count`
- `ide_tags` --- writes `session.ide_tags`
- `model` --- writes `session.model`
- `duration` --- writes `session.duration_ms`
- `sidechain` --- writes `session.sidechain`
- `compact` --- writes `session.compact`
- `api_error` --- writes `session.api_error`
- `tools` --- writes `session.tool_use`, `session.tool_result`,
  `session.tool_error`
- `attachments` --- writes `session.attachment`, `session.max_turns_reached`

`tools` and `attachments` are grouped because their notes share computation:
`tool_error` needs a `tool_use_id` to tool-name map built from `tool_use`
blocks, and `max_turns_reached` is one key of the same attachment-entry scan.
Other tasks are independent.
