---
title: gage blame
description:
  Command reference for gage blame, which finds the sessions and tool calls
  that wrote the lines of a file
---

The `gage blame` command finds the sessions that wrote the lines of a file. For
each line it reports the session, the tool call that wrote the line, and the
lines of the session to read. For a walk-through, see
[_See the tool call that edited a line_](/docs/how-to/blame).

```cli
gage blame [OPTIONS] <PATH>
```

| Option                       | Description                               |
| ---------------------------- | ----------------------------------------- |
| `-l, --lines <LINES>`        | Lines to blame (default: all)             |
| `-N, --native <SOURCE>`      | Session source                            |
| `-S, --stored`               | Use stored sessions                       |
| `-R, --stored-remote <NAME>` | Use sessions fetched from a remote        |
| `--json`                     | Output JSON                               |

`--lines` takes a line (`42`), a range (`10-20`), or a comma-separated list of
them (`10-20,42`). Lines are numbered from 1 and a range includes both ends.

`--native`, `--stored`, and `--stored-remote` select the sessions to read, and
each excludes the other two. With none of them, the command reads the default
session source. They have the meaning they have for
[`gage session`](/commands/session).

## How a line is attributed

The command follows `git blame`. The commit of each line bounds a window of
time. The earliest tool call in that window whose input carries the text of the
line is the call that wrote it. A line that no tool call carries is not
attributed.

## Output

Each line of the file is one row.

```text
113509cd:432-1113  2026-11-20 11:16  116  ### Gage in Pi
-                  -                 117
113509cd:432-1113  2026-11-20 11:16  118  [`gage init`](/commands/init) sets Gage up in Pi, and Pi can be the
```

| Column | Description                                                        |
| ------ | ------------------------------------------------------------------ |
| 1      | The session and the session lines to read, as `SESSION:LINES`      |
| 2      | The time of the tool call                                          |
| 3      | The line number in the file                                        |
| 4      | The line                                                           |

The session lines run from the human prompt before the tool call through the
call itself. [`gage session view`](/commands/session) takes the first
column as its argument and opens the session at those lines. A `-` marks a
line that is not attributed.

## JSON output

`--json` writes one object for each range of lines attributed to one tool
call. It includes every candidate call for a line, where the text output shows
only the first.

| Field           | Description                                              |
| --------------- | -------------------------------------------------------- |
| `line`          | First line of the range                                  |
| `line_end`      | Last line of the range. Absent for a single line.        |
| `session_id`    | Session that holds the tool call                         |
| `session_lines` | Session lines to read                                    |
| `tool_use_id`   | ID of the tool call                                      |
| `tool_name`     | Name of the tool                                         |
| `timestamp`     | Time of the tool call                                    |
| `rank`          | Rank of the candidate. The first is `1`.                 |
| `match`         | Evidence for the attribution                             |

`match` is one of `patch`, `input`, `command`, or `neighbor`, from the
strongest evidence to the weakest.

The same rows are available in SQL through the `blame` table function. See
[`gage query`](/commands/query).
