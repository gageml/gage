---
title: gage session
description:
  Command reference for gage session, including subcommands to list, view,
  move, and delete sessions
---

The `gage session` command manages sessions. For background, see
[Sessions](/docs/sessions).

```cli
gage session [OPTIONS] <COMMAND>
```

Commands that take session IDs accept unique ID prefixes. The `-A, --agent`
option, given before the subcommand, operates on agent sessions instead of
Claude Code sessions.

## session list

List available sessions.

```cli
gage session list [OPTIONS]
```

| Option             | Description                                             |
| ------------------ | ------------------------------------------------------- |
| `-m, --more`       | Show more items. Repeat to show more per use.           |
| `-a, --all`        | Show all items                                          |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20)        |
| `--project <PATH>` | Filter by project path (repeatable)                     |
| `--since <SINCE>`  | Filter by how long ago the session was modified (e.g. `1h`, `30m`, `7d`) |
| `--empty`          | Only show empty sessions                                |
| `--full-id`        | Show the full session ID, never truncating it           |
| `-S, --stats`      | Include additional stats columns: model time, tokens (in / out / cached), turns |

## session view

View a session.

```cli
gage session view [OPTIONS] [SESSION]
```

If you omit the ID, Gage lets you pick from a list.

| Option                  | Description                    |
| ----------------------- | ------------------------------ |
| `-v, --options <OPTIONS>` | View options (comma-separated) |

View options: `turns` shows model turns in the outline. `detail` shows all
entries. The default hides low-signal entries.

## session move

Move a session to a different project directory.

```cli
gage session move [OPTIONS] <SESSION> <DIR>
```

The destination project directory must exist.

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

## session delete

Delete sessions.

```cli
gage session delete [OPTIONS] [IDS]...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `--empty`   | Delete empty sessions    |
| `-y, --yes` | Skip confirmation prompt |
