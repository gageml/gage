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

## Authors and duplicate detection

Every note records an `author` — the identity of its writer. A note's
duplicate key is `(name, target, author)`: the same writer saying the same
thing about the same target is a duplicate, detected at write time. The
writer resolves a duplicate by erroring (default), replacing the previous
note, or keeping it.

The name identifies *what kind* of note it is; the author identifies *who
wrote it*. Author values follow a small scheme:

- `scanner:<name>` — a scanner writing deterministic notes. The author is
  fixed, so re-writing the same note for the same target is detected and
  resolved (typically replaced).
- `agent:<scanner>?call=<toolUseId>` — a model writer under an agent run
  started by a scanner via `call_agent`. The author is the authoring
  call: `toolUseId` is the tool-use block id from the calling session's
  transcript, so every written item ties back to the exact transcript
  entry that wrote it, and distinct calls (including re-scans and
  parallel agents) write side by side without conflict. A duplicate can
  only mean the same call wrote twice — a retry, correctly rejected as
  already-written. Scanner-defined tools derive this value with
  `meta.to_author()` (on the second tool-function argument, which
  otherwise holds the MCP request's `_meta` verbatim) and pass it to
  `write_note` / `write_issue`.
- `agent:<client>@<version>?call=<toolUseId>` — an ad-hoc MCP client
  (e.g. the `gage mcp` stdio server used by a Claude Code session);
  same authoring-call semantics, with the caller named by its client
  info.
- `user:<username>` — a person writing through the CLI. Where each write
  is its own event (e.g. `gage issue add`), a `?call=<id>` qualifier
  carries the event identity.

Because agent authors carry instance identity, note names stay purely
type identifiers — all findings are named `finding`, and readers select
them by exact name.
