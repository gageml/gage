---
title: Notes
description:
  What a note is, what a note can target, and how note names, authors, and
  carry forward work
---

_Notes_ are values that scanners and people assign to sessions, session lines,
attachments, and other stored objects. Scanner notes are the supporting evidence
for issues.

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

## Targets

A note's _target_ is the object the note is about. A note can have no target.
What a note can target depends on how it is written:

- A scanner's `write_note` targets a session, lines of a session, or an
  attachment.
- The `note_write` tool targets a session or lines of a session.
- A note added in the session viewer targets the session or one of its
  lines.
- `gage note add --target` targets any stored object by ID or unique prefix: a
  session, an attachment, a dataset, a scan, an issue, or another note. A line
  selection applies to a session only.

The `target` column of the `note` table holds the target as `<type>:<id>`, as in
`session:<id>` or `issue:<id>`. Cited lines follow a session ID after `#`, as in
`session:<id>#12-20`.

## Names and docs

A note name identifies what kind of note it is. All user comments are named
`comment`. A scanner declares the names it writes along with a _note doc_, a
description of what those notes contain and how they were produced. Downstream
scanners select notes by name pattern. A finding note from any scanner is
named `<kind>[finding]`, and the `findings` scanner reads every note matching
`*[finding]`.

`gage note show --doc` prints a note's doc.

## Authors

Every note records an `author`, the identity of its writer. Nothing constrains
`(name, target, author)`: the same writer may say the same thing about the same
target any number of times, and a plain write always inserts.

- `task:<scanner>:<task>` is a scanner task. The runtime sets this author on
  every note a task writes, including notes written through a tool an agent
  called.
- `user:<username>` is a person writing through the CLI, the TUI, or the Gage
  tools in a Claude Code session.

## Carry forward

A scanner avoids re-reading what an earlier scan already covered with two
mechanisms. A _watermark_ records, under a key the scanner chooses, how far a
task got on a session, note, or attachment. A later scan asks for the _unseen_
part and reads only that: a session that grew since the last scan is read over
its new lines only. A note written with a carry-forward key is linked into a
later scan when its target is in that scan, so the later scan's downstream
tasks see it without the note being written again.

`gage scan --invalidate` ignores both: every session is scanned in full and
nothing is carried forward.
