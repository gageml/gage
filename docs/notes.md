---
title: Notes
---

_Notes_ are values that scanners assign to sessions, session lines, config
settings, and other topics as support evidence for issues.

Scanners may write a lot of notes as they run. This information persists across
scans. It's helpful to review notes over time before concluding there's an
issue worth reporting.

List notes:

```cli
gage note list
```

To view note details, run:

```cli
gage note show <NOTE_ID>
```

You can delete notes using `gage note delete`. In general this is not needed.
Scanners benefit from having this note record across scans.

## Authors and duplicate policies

Every note records an `author` — the identity of its writer. Nothing
constrains `(name, target, author)`: the same writer may say the same
thing about the same target any number of times, and a plain write
always inserts. A writer that wants different behavior states it as a
policy on the write: `replace_prev` overwrites its most recent earlier
note with that name and target (deleting any older ones), and
`keep_prev` leaves the earlier note in place and returns it. This is how
scanners keep re-scans from accumulating copies of the same note.

The name identifies *what kind* of note it is; the author identifies *who
wrote it*. Author values follow a small scheme:

- `scanner:<name>` — a scanner writing deterministic notes. The author is
  fixed, so `replace_prev` / `keep_prev` match the prior note across
  runs.
- `agent:<scanner>?call=<toolUseId>` — a model writer under an agent run
  started by a scanner via `call_agent`. The author is the authoring
  call: `toolUseId` is the tool-use block id from the calling session's
  transcript, so every written item ties back to the exact transcript
  entry that wrote it. Scanner-defined tools derive this value with
  `meta.agent_tool_use()` (on the second tool-function argument, which
  otherwise holds the MCP request's `_meta` verbatim) and pass it to
  `write_note` / `write_issue`.
- `agent:<client>@<version>?call=<toolUseId>` — an ad-hoc MCP client
  (e.g. the `gage mcp` stdio server used by a Claude Code session);
  same authoring-call semantics, with the caller named by its client
  info.
- `user:<username>` — a person writing through the CLI or TUI.

Note names stay purely type identifiers — all findings are named
`finding`, all user comments are named `comment` — and readers select
them by exact name.
