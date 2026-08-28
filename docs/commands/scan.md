---
title: gage scan
description:
  Command reference for gage scan, which runs scanners on sessions, and its
  subcommands to list, view, delete, and invalidate scan runs
---

The `gage scan` command runs scanners on sessions. For background, see
[Scans](/docs/scans) and [Scanners](/docs/scanners).

```cli
gage scan [OPTIONS] [SESSION]...
gage scan <COMMAND>
```

With no session selection options, the command scans sessions from the last 30
days, up to 50. Session arguments accept unique ID prefixes.

| Option                  | Description                                        |
| ----------------------- | -------------------------------------------------- |
| `-A, --agent`           | Operate on agent sessions instead of Claude Code sessions |
| `-p, --project <PROJECT>` | Limit sessions to a project, given as a directory path or a project slug as shown by `gage session list` |
| `-s, --scanner <NAME>`  | Scanner to run (repeatable)                        |
| `-g, --group <NAME>`    | Run the scanners in a group (repeatable)           |
| `-f, --file <PATH>`     | Scanner file to run (repeatable)                   |
| `-n, --limit <N>`       | Scan the latest N sessions                         |
| `-r, --sample <N>`      | Scan N sessions selected at random                 |
| `-d, --days <N>`        | Scan sessions from past N days                     |
| `-t, --today`           | Scan sessions modified since midnight local time   |
| `-a, --all`             | Scan all sessions                                  |
| `--rerun <SCAN>`        | Re-run a scan's scanners on its sessions           |
| `--scan <SCAN>`         | Scan a scan's agent sessions                       |
| `-i, --invalidate`      | Invalidate the selected sessions before scanning   |
| `-y, --yes`             | Skip confirmation prompt                           |
| `-j, --jobs <N>`        | Maximum concurrent tasks (defaults to number of CPUs) |
| `--agent-jobs <N>`      | Maximum concurrent agents (default 8)              |
| `--no-progress`         | Don't show progress                                |
| `--list-scanners`       | Show available scanners and exit                   |

`--limit` combines with `--days` or `--today` to cap the sessions selected from
the window. `--sample` draws from sessions modified in the past 30 days, or the
window given with `--days` or `--today`.

`--scan` expands to the agent sessions the given scan's tasks spawned and
selects the `eval` scanner group. `--limit` and `--sample` cap the expanded
list.

`--invalidate` clears the selected sessions' validation state so tasks re-run
for them.

## scan list

List scan runs.

```cli
gage scan list [OPTIONS]
```

| Option              | Description                                   |
| ------------------- | --------------------------------------------- |
| `-m, --more`        | Show more items. Repeat to show more per use. |
| `-a, --all`         | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |

## scan view

View a scan run in the scan TUI.

```cli
gage scan view [SCAN_ID]
```

If you omit the ID, Gage lets you pick from a list.

## scan delete

Delete scan runs and their associated notes.

```cli
gage scan delete [OPTIONS] [IDS]...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

## scan invalidate

Invalidate task validation state. Invalidated tasks re-run on the next
applicable scan.

```cli
gage scan invalidate [OPTIONS] <--session <ID>|--note <ID>|--task <NAME>>
```

| Option              | Description                                            |
| ------------------- | ------------------------------------------------------ |
| `-s, --session <ID>` | Invalidate tasks for a session (ID or prefix, repeatable) |
| `-n, --note <ID>`   | Invalidate tasks for a note (ID or prefix, repeatable) |
| `-t, --task <NAME>` | Invalidate tasks by name (or prefix, repeatable)       |
| `-y, --yes`         | Skip confirmation prompt                               |
