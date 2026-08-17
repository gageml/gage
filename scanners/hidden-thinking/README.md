# hidden-thinking

Detects sessions where model thinking is hidden. Claude Code's default
settings replace thinking text with summaries, leaving empty thinking
blocks in the transcript.

- `thinking.empty` (note) --- `true` if the thinking block's text is
  empty, otherwise `false`
- `hidden-thinking` (issue) --- opened when the latest observation is
  `true`; description in `issue-description.md`

## Behavior

The note task writes at most **one note per scan**: it finds the single
newest thinking block across all sessions and records whether its text
is empty. The early-exit in `note()` relies on sessions being ordered
newest-modified first. Note metadata freezes the observation context -
block timestamp, model, and the `showThinkingSummaries` config value at
observation time.

The issue task sorts all `thinking.empty` notes by observed block
timestamp and acts on the newest. `value: false` notes are consumed by
nothing today; they exist as observation history.

## Design: why the note/issue split stays

The split looks like overkill - one producer, one consumer, a single
note per scan. It has been reviewed and kept deliberately. A merged
single-task version (scan sessions, decide, `write_issue` directly) is
mechanically possible: `write_issue` accepts a `sessions` field that
tags sessions without any note. But that path gives up three things:

1. **Line anchor.** The note targets `#{ session, line }`, pinpointing
   the thinking block. The `sessions` field is session IDs only.

2. **Observation snapshot.** After the user fixes their config (or
   session retention prunes the session file), the raw session can no
   longer show what was true when the empty block was produced. The
   note metadata can.

3. **Reopen on regression.** `open_on_new_evidence` reopens a closed
   issue by comparing incoming vs. recorded *evidence* timestamps, and
   evidence entries are notes. An evidence-free duplicate issue write
   changes nothing, so it cannot reopen the issue. The note is the
   carrier of "this observation is new."

Point 3 is the load-bearing one: the intended lifecycle is user closes
the issue after fixing config, config later regresses, a fresh empty
block produces a new note, and the issue reopens. This scanner is the
model for that reopen-on-new-evidence scheme.

See [scanner docs](docs/index.md) for more information.
