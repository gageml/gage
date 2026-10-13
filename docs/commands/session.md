---
title: gage session
description:
  Command reference for gage session, including subcommands to list, add,
  remove, delete, and view sessions
---

The `gage session` command manages sessions. For background, see
[Sessions](/docs/sessions).

```cli
gage session [OPTIONS] <COMMAND>
```

A session is either _native_, a file in a session source such as the Claude
Code home directory, or _stored_, a copy in the Gage store. Commands that take
session IDs accept unique ID prefixes.

Two options select which sessions a subcommand works on. They are accepted
before or after the subcommand.

| Option                  | Description                       |
| ----------------------- | --------------------------------- |
| `-s, --source <SOURCE>` | Session source                    |
| `-S, --stored`          | Operate on stored sessions        |

A source is `SCHEME:PATH`. A value with no scheme uses the default driver.
The `claude` scheme reads Claude Code sessions under `PATH/projects`, or the
Claude Code home directory when `PATH` is omitted.

## session list

List available sessions.

```cli
gage session list [OPTIONS]
```

| Option                    | Description                                              |
| ------------------------- | -------------------------------------------------------- |
| `-p, --project <PROJECT>` | Filter by project path or name                           |
| `--since <SINCE>`         | Show sessions modified within a duration (e.g. `1h`, `30m`, `7d`) |
| `--empty`                 | Only show empty sessions                                 |
| `-d, --dataset <DATASET>` | List sessions in a dataset, in member order. Implies `--stored`. |
| `-m, --more`              | Show more items. Repeat to show more per use.            |
| `-a, --all`               | Show all items                                           |
| `-n, --limit <LIMIT>`     | Limit the number of items shown (default: 20)            |
| `--full-id`               | Show the full session ID                                 |

## session add

Add sessions to the store.

```cli
gage session add [OPTIONS] [SESSION]...
```

Each session is read from the source and written as a session object. Adding a
session that is already in the store updates it when its content changed. At
least one session argument or selection option is required.

| Option                    | Description                                                  |
| ------------------------- | ------------------------------------------------------------ |
| `-d, --dataset <DATASET>` | Add the sessions to a dataset in the same step. With `--stored`, links sessions already in the store. |
| `-p, --project <PROJECT>` | Limit sessions to a project path or name                     |
| `--days <N>`              | Select sessions modified in past N days                      |
| `--today`                 | Select sessions modified since midnight local time           |
| `-a, --all`               | Select every available session                               |
| `-n, --limit <N>`         | Select the latest N sessions                                 |
| `-r, --sample <N>`        | Select N sessions at random                                  |
| `--max-size <SIZE>`       | Maximum stored size per session, e.g. `256MB` or `1GB`. Overrides `storage.max_session_size`. |
| `-y, --yes`               | Skip confirmation prompt                                     |

## session remove

Remove stored sessions from the store or from a dataset.

```cli
gage session remove [OPTIONS] <IDS>...
```

Without `--dataset`, each session is unlinked from every dataset that holds it
and marked removed in the store. With `--dataset`, each session is unlinked
from that dataset only and stays in the store. The native session is never
affected.

| Option                    | Description                                 |
| ------------------------- | ------------------------------------------- |
| `-d, --dataset <DATASET>` | Remove from this dataset only (ID or prefix) |
| `-y, --yes`               | Skip confirmation prompt                    |

## session delete

Delete native sessions. Each session's files are removed from the source.
Stored sessions are not affected.

```cli
gage session delete [OPTIONS] [IDS]...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `--empty`   | Delete empty sessions    |
| `-y, --yes` | Skip confirmation prompt |

## session view

View a session.

```cli
gage session view [OPTIONS] [SESSION]
```

If you omit the ID, Gage lets you pick from a list.

| Option                    | Description                    |
| ------------------------- | ------------------------------ |
| `-v, --options <OPTIONS>` | View options (comma-separated) |

View options: `turns` shows model turns in the outline. `detail` shows all
entries. The default hides low-signal entries.
