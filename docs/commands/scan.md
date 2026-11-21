---
title: gage scan
description:
  Command reference for gage scan, which runs scanners on sessions, and its
  subcommands to list, view, and delete scan runs
---

The `gage scan` command runs scanners on sessions. For background, see
[Scans](/docs/scans) and [Scanners](/docs/scanners).

```cli
gage scan [OPTIONS] [SESSION]...
gage scan <COMMAND>
```

A scan runs over a dataset. With session selection options, the command adds
the selected sessions to the store, creates a dataset holding them, and scans
it. With `--dataset`, the command scans an existing dataset. Session arguments
accept unique ID prefixes.

With no scanner or session options, the command prompts for scanners and
sessions. With `--yes`, unspecified selections take defaults: the `default`
scanner group, and sessions from the past 30 days capped at 20.

| Option                          | Description                                                         |
| ------------------------------- | ------------------------------------------------------------------- |
| `-N, --native <SOURCE>`             | Session source, as described in [`gage session`](/docs/commands/session) |
| `-S, --stored`                  | Scan stored sessions                                                |
| `-R, --stored-remote <NAME>`    | Scan sessions fetched from a remote                                 |
| `-s, --scanner <SCANNER>`       | Scanner to run, as a name or path (repeatable)                      |
| `-g, --group <NAME>`            | Scanner group to run (repeatable)                                   |
| `-d, --dataset <DATASET>`       | Scan an existing dataset (ID or prefix)                             |
| `-p, --project <PROJECT>`       | Limit sessions to a project, given as a directory path or a project slug as shown by `gage session list` |
| `--touches <DIR>`               | Limit sessions to those that touch a directory                      |
| `--since <SINCE>`               | Select sessions active within a duration (e.g. `1h`, `30m`, `7d`)   |
| `-a, --all`                     | Select every available session                                      |
| `-n, --limit <N>`               | Select the latest N sessions                                        |
| `-r, --sample <N>`              | Select N sessions at random                                         |
| `-j, --jobs <N>`                | Tasks to run at once (default 10)                                   |
| `--agent-jobs <N>`              | Agents to run at once across all tasks (default 8)                  |
| `--invalidate`                  | Ignore prior work: scan every session in full and carry no notes forward |
| `--ignore-missing-attachments`  | Start the scan when a required attachment is missing                |
| `--ignore-missing-harness`      | Start the scan when the default driver's harness is missing         |
| `-t, --tag <NAME>`              | Tag the new scan run                                                |
| `--force-tag`                   | Move the tag if it exists                                           |
| `--no-progress`                 | Print task output as it happens instead of the progress view        |
| `--no-dataset`                  | Run with no dataset; scanners see no sessions                       |
| `--list-scanners`               | Show available scanners and exit                                    |
| `-y, --yes`                     | Skip prompts and confirmation                                       |

`--scanner` takes a scanner name. When no scanner has that name, the value is a
path to a directory holding a `scanner.rn` or to a scanner file. A `#{...}`
suffix overrides the scanner's params.

A session touches a directory when it wrote or read a file in the directory or
below it, or when it started there.

`--limit` combines with `--since` to cap the sessions selected from the window.
`--sample` draws from sessions active in the past 30 days, or the window given
with `--since`.

A stored session is as recent as its last entry, or as the store's last write
of it when no entry has a time. A native session is as recent as the last
modification of its file. `--since`, `--limit`, and `--sample` all use this
time.

A scan runs the attach tasks of the selected scanners on a dataset it creates.
An existing dataset passed with `--dataset` is scanned as it is; use
[`gage attachment add --scanner`](/docs/commands/attachment#attachment-add)
to prepare it. `--ignore-missing-attachments` lets a task that needs an
attachment the dataset lacks run and fail on its own. Without it, the scan does
not start.

A scan asks the default driver whether its harness, such as Claude Code, Codex,
or Pi, is installed and logged in before it starts. `--ignore-missing-harness`
skips that question, so a scan whose tasks start no agents runs without a
harness. An agent that a task starts on a missing harness fails on its own.
Without the option, the scan does not start.

A scan agent on Pi is not sandboxed. See
[Known issues](https://github.com/gageml/gage/blob/main/KNOWN_ISSUES.md#pi).

`--stored-remote` names a remote of the store. The command selects among the
sessions that [`gage fetch`](/docs/commands/fetch) brought from that remote and
that are not merged into the store. It fetches nothing, and a remote that
nothing was fetched from is an error that names `gage fetch`. The session
selection options apply to the fetched sessions. The scan's dataset, notes, and
issues are written to the store. The dataset links each fetched session at the
commit the remote holds, and the session stays apart from the store's own
sessions. `--stored-remote` excludes `--native`, `--stored`, `--dataset`, and
`--no-dataset`.

An object that links a fetched session is withheld:
[`gage push`](/docs/commands/push) refuses it and
[`gage backup`](/docs/commands/backup) leaves it out, because sending it would
send the remote's session. The dataset of such a scan is withheld, and so are
its notes on the sessions, the issues that cite those notes, and the scan
itself. Merging the sessions with [`gage merge`](/docs/commands/merge) ends the
withholding.

```cli
gage fetch acme --all
gage scan --stored-remote acme --since 7d
```

`--no-progress` is the output when stdout is not a terminal.

## scan list

List scan runs.

```cli
gage scan list [OPTIONS]
```

| Option                | Description                                   |
| --------------------- | --------------------------------------------- |
| `-m, --more`          | Show more items. Repeat to show more per use. |
| `-a, --all`           | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |
| `--full-id`           | Show the full scan run ID                     |

## scan view

View a scan run in the scan TUI. The view shows the scan's tasks, sessions,
issues, and notes.

```cli
gage scan view [SCAN]
```

If you omit the ID, Gage lets you pick from a list.

## scan delete

Delete scan runs. Each scan run is deleted along with the notes and issues it
wrote. Notes carried forward from earlier scan runs are kept.

A note or an issue that the scan run wrote is kept when another scan run or an
issue still uses it. This applies to an issue that another scan run also wrote,
to a note that a later scan run carried forward, and to a note that an issue
cites. A kept note or issue is not changed. The command reports the number of
notes and issues it deleted and the number it kept.

```cli
gage scan delete [OPTIONS] <IDS>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
