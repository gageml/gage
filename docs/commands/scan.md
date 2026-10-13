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
| `-s, --scanner <SCANNER>`       | Scanner to run, as a name or path (repeatable)                      |
| `-g, --group <NAME>`            | Scanner group to run (repeatable)                                   |
| `-d, --dataset <DATASET>`       | Scan an existing dataset (ID or prefix)                             |
| `-p, --project <PROJECT>`       | Limit sessions to a project, given as a directory path or a project slug as shown by `gage session list` |
| `--days <N>`                    | Select sessions modified in past N days                             |
| `--today`                       | Select sessions modified since midnight local time                  |
| `-a, --all`                     | Select every available session                                      |
| `-n, --limit <N>`               | Select the latest N sessions                                        |
| `-r, --sample <N>`              | Select N sessions at random                                         |
| `-j, --jobs <N>`                | Tasks to run at once (default 10)                                   |
| `--agent-jobs <N>`              | Agents to run at once across all tasks (default 8)                  |
| `--invalidate`                  | Ignore prior work: scan every session in full and carry no notes forward |
| `--ignore-missing-attachments`  | Start the scan when a required attachment is missing                |
| `--tag <NAME>`                  | Tag the new scan run                                                |
| `--force-tag`                   | Move the tag if it exists                                           |
| `--no-progress`                 | Print task output as it happens instead of the progress view        |
| `--no-dataset`                  | Run with no dataset; scanners see no sessions                       |
| `--list-scanners`               | Show available scanners and exit                                    |
| `-y, --yes`                     | Skip prompts and confirmation                                       |

`--scanner` takes a scanner name. When no scanner has that name, the value is a
path to a directory holding a `scanner.rn` or to a scanner file. A `#{...}`
suffix overrides the scanner's params.

`--limit` combines with `--days` or `--today` to cap the sessions selected from
the window. `--sample` draws from sessions modified in the past 30 days, or the
window given with `--days` or `--today`.

A scan runs the attach tasks of the selected scanners on a dataset it creates.
An existing dataset passed with `--dataset` is scanned as it is; use
[`gage attachment add --scanner`](/docs/commands/attachment#attachment-add)
to prepare it. `--ignore-missing-attachments` lets a task that needs an
attachment the dataset lacks run and fail on its own. Without it, the scan does
not start.

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

```cli
gage scan delete [OPTIONS] <IDS>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
