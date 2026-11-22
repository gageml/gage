---
title: See the tool call that edited a line
description:
  How to use gage blame to go from a line of code to the session and tool call
  that wrote it
---

`git blame` tells you which commit last changed a line. `gage blame` tells you
which tool call wrote it, and in which session. From there you open the session
at the prompt that led to the edit and read what was asked, what your agent
reasoned, and what it changed. You do not search for the session. You start
from the line.

To ask your agent the same thing in conversation, see
[_Recall code decisions and reasoning_](/how-to/recall).

## Before you start

Install Gage and run `gage init`. See [Quick start](/start). The file must
be in a Git repository, because Gage uses each line's commit to narrow the
search for the tool call.

## Steps

1. Run `gage blame` on the file. Use `-l` to select lines.

   ```cli
   gage blame docs/sessions.md -l 116-120
   ```

2. Read the output. Each line of the file is shown with the session that wrote
   it, the lines of that session to read, and the time of the tool call.

   ```text
   113509cd:432-1113  2026-11-20 11:16  116  ### Gage in Pi
   -                  -                 117
   113509cd:432-1113  2026-11-20 11:16  118  [`gage init`](/commands/init) sets Gage up in Pi, and Pi can be the
   113509cd:432-1113  2026-11-20 11:16  119  default driver. Gage works in Pi as it does in Claude Code, with these
   113509cd:432-1113  2026-11-20 11:16  120  differences:
   ```

   `113509cd` is the session. `432-1113` is the part of the session that runs
   from your prompt to the tool call that wrote the line. A `-` means that no
   session wrote the line.

3. Open the session at those lines. Copy the first column as it is.

   ```cli
   gage session view 113509cd:432-1113
   ```

   The view opens at your prompt. Read forward to the tool call.

## Find the tool call itself

Add `--json` to get the ID and the name of the tool call for each line.

```cli
gage blame docs/sessions.md -l 116 --json
```

```json
[
  {
    "line": 116,
    "session_id": "113509cd-ab67-4bc9-aa15-d5702dd084f1",
    "session_lines": "432-1113",
    "tool_use_id": "toolu_011kGu7xqrUaXgYGMcqw1tyo",
    "tool_name": "Bash",
    "timestamp": "2026-11-20T11:16:44.536Z",
    "rank": 1,
    "match": "command"
  }
]
```

## Sessions that are gone

`gage blame` reads the sessions your coding agent keeps on disk. When your
coding agent has deleted a session, read the copies in the Gage store:

```cli
gage blame --stored docs/sessions.md -l 116-120
```

See [`gage blame`](/commands/blame) for every option.
