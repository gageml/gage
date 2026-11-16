---
title: gage session
description:
  Command reference for gage session, including subcommands to list, add,
  delete, move, view, and find sessions
---

The `gage session` command manages sessions. For background, see
[Sessions](/docs/sessions).

```cli
gage session [OPTIONS] <COMMAND>
```

A session is either _native_, a file in a session source such as the Claude
Code home directory, or _stored_, a copy in the Gage store. Commands that take
session IDs accept unique ID prefixes.

Three options select which sessions a subcommand works on. They are accepted
before or after the subcommand, and each excludes the other two.

| Option                       | Description                                 |
| ---------------------------- | ------------------------------------------- |
| `-N, --native <SOURCE>`      | Session source                              |
| `-S, --stored`               | Operate on stored sessions                  |
| `-R, --stored-remote <NAME>` | Operate on sessions fetched from a remote   |

`--stored-remote` names a remote of the store. The sessions are those that
[`gage fetch`](/docs/commands/fetch) brought from that remote, which are kept
apart from the store's own sessions until
[`gage merge`](/docs/commands/merge) merges them. Nothing is fetched and
nothing is written. `list`, `view`, and `find` accept the option. `add`,
`delete`, and `move` refuse it, because fetched sessions are read only. With
`list --dataset` or `list --share`, the dataset or share is one the remote
holds.

A source is `SCHEME:PATH`. A value with no scheme uses the default driver,
which the `drivers.default` setting names; see
[`gage config`](/docs/commands/config#settings).
The `claude` scheme reads Claude Code sessions under `PATH/projects`, or the
Claude Code home directory when `PATH` is omitted. The `codex` scheme reads
Codex sessions under `PATH/sessions`, or the Codex home directory when `PATH`
is omitted. See [Codex sessions](/docs/sessions#codex-sessions).

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
| `--share <SHARE>`         | List sessions in a share, given as a tag, ID, or prefix. Implies `--stored`. |
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
| `--share <SHARE>`         | Add the sessions to a share in the same step, given as a tag, ID, or prefix. A share that names filters admits published sessions only. |
| `-p, --project <PROJECT>` | Limit sessions to a project path or name                     |
| `--touches <DIR>`         | Limit sessions to those that touch a directory               |
| `--since <SINCE>`         | Select sessions modified within a duration (e.g. `1h`, `30m`, `7d`) |
| `-a, --all`               | Select every available session                               |
| `-n, --limit <N>`         | Select the latest N sessions                                 |
| `-r, --sample <N>`        | Select N sessions at random                                  |
| `--max-size <SIZE>`       | Maximum stored size per session, e.g. `256MB` or `1GB`. Overrides `storage.max_session_size`. |
| `-t, --tag <NAME>`        | Tag the added session. Applies when one session is given.    |
| `--force-tag`             | Move the tag if it exists                                    |
| `-y, --yes`               | Skip confirmation prompt                                     |

A session touches a directory when it wrote or read a file in the directory or
below it, or when it started there.

`--limit` combines with `--since` to cap the sessions selected from the window.
`--sample` draws from sessions modified in the past 30 days, or the window given
with `--since`.

## session delete

Delete native sessions, or stored sessions with `--stored`.

```cli
gage session delete [OPTIONS] [IDS]...
```

Each native session's files are removed from the source. Stored sessions are
not affected.

With `--stored`, each session is unlinked from every dataset and share that
holds it and marked removed in the store. The native session is not affected.
To take a session out of one dataset and keep it in the store, use
[`gage dataset unlink`](/docs/commands/dataset#dataset-unlink).

| Option      | Description                                      |
| ----------- | ------------------------------------------------ |
| `--empty`   | Delete empty sessions. Applies to native sessions. |
| `-y, --yes` | Skip confirmation prompt                         |

A native Codex session is not deleted; the command reports that it is not
supported.

## session move

Move a native session to another project.

```cli
gage session move [OPTIONS] <SESSION> <DIR>
```

The session's files are moved to the project at `DIR`, and `DIR` is recorded as
the session's working directory, so Claude Code resumes the session from there.
Stored sessions are not affected. A session that is open in Claude Code is not
moved. A Codex session is not moved; the command reports that it is not
supported.

| Option      | Description                                         |
| ----------- | --------------------------------------------------- |
| `--force`   | Move the session without checking that it is closed |
| `-y, --yes` | Skip confirmation prompt                            |

## session view

View a session.

```cli
gage session view [OPTIONS] [SESSION]
```

If you omit the ID, Gage lets you pick from a list. A trailing `:LINES` opens
the view at a session line, as in `7260:2956`. `LINES` is a line (`42`), a
range (`10-20`), or a comma-separated list of them (`10-20,42`). The view opens
at the first line.

| Option                    | Description                    |
| ------------------------- | ------------------------------ |
| `-v, --options <OPTIONS>` | View options (comma-separated) |

Each view option has a short and a long name. A leading `!` turns an option
off, as in `!user`.

| View option      | Description                                         |
| ---------------- | --------------------------------------------------- |
| `a`, `assistant` | Assistant messages and thinking in the outline (on by default) |
| `u`, `user`      | User messages in the outline (on by default)        |
| `t`, `tools`     | Tool calls in the outline                           |
| `e`, `entries`   | All entries in the outline, including system and bookkeeping entries |
| `s`, `source`    | Native source in the content pane                   |
| `n`, `turns`     | Turn markers in the outline                         |
| `c`, `changes`   | Changes of a published session                      |

## session find

Find sessions by message text.

```cli
gage session find [OPTIONS] <QUERY>
```

The command searches message text in the default source, in each `--native`,
in the store with `--stored`, and in the sessions fetched from each
`--stored-remote`. It lists the matching messages, best match first. `--native`
and `--stored-remote` are repeatable here, and the three options combine.

Bare terms match any of them. `a AND b`, `+a`, `-a`, a quoted phrase, and a
field term such as `type:assistant` narrow a search.

| Option                | Description                                         |
| --------------------- | --------------------------------------------------- |
| `--since <SINCE>`     | Search sessions modified within a duration (e.g. `1h`, `30m`, `7d`) |
| `-U, --user`          | Search text typed by the user                       |
| `-A, --assistant`     | Search text written by the assistant                |
| `-T, --tool`          | Search tool calls and results                       |
| `-D, --by-date`       | Sort hits by date, most recent first. The hits shown are still the best matches. |
| `-m, --more`          | Show more items. Repeat to show more per use.       |
| `-a, --all`           | Show all items, up to 1000 matches per source       |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20)       |
| `--full-id`           | Show the full session ID                            |

`--user`, `--assistant`, and `--tool` search only messages of those kinds.
`--user` with `--assistant` searches both.
