---
title: gage dataset
description:
  Command reference for gage dataset, including subcommands to add, refresh,
  list, show, and delete datasets
---

The `gage dataset` command manages datasets. A dataset is a set of stored
sessions and attachments that a scan runs over. `gage scan` creates a dataset
from its session selection unless given one with `--dataset`. For background,
see [Scans](/docs/scans).

```cli
gage dataset <COMMAND>
```

Commands that take dataset IDs accept unique ID prefixes and tag names.

## dataset add

Add a dataset.

```cli
gage dataset add [OPTIONS] [SESSION]...
```

Without session arguments or selection options, the command adds an empty
dataset. With them, the selected native sessions are added to the store and
linked into the new dataset in a single step.

| Option                    | Description                                                  |
| ------------------------- | ------------------------------------------------------------ |
| `-s, --source <SOURCE>`   | Session source, as described in [`gage session`](/docs/commands/session) |
| `-p, --project <PROJECT>` | Limit sessions to a project path or name                     |
| `--days <N>`              | Select sessions modified in past N days                      |
| `--today`                 | Select sessions modified since midnight local time           |
| `-a, --all`               | Select every available session                               |
| `-n, --limit <N>`         | Select the latest N sessions                                 |
| `-r, --sample <N>`        | Select N sessions at random                                  |
| `--max-size <SIZE>`       | Maximum stored size per session. Overrides `storage.max_session_size`. |
| `--tag <NAME>`            | Tag the new dataset                                          |
| `--force-tag`             | Move the tag if it exists                                    |
| `-y, --yes`               | Skip confirmation prompt                                     |

To add sessions to an existing dataset, use
[`gage session add --dataset`](/docs/commands/session#session-add). To add
attachments, use
[`gage attachment add --dataset`](/docs/commands/attachment#attachment-add).

## dataset refresh

Re-add each dataset session from its source.

```cli
gage dataset refresh [OPTIONS] <DATASET>
```

Every member is re-read from the source. A session whose native content grew
is updated in place. This is equivalent to running `gage session add -d
DATASET` on every member's native ID.

| Option                  | Description                                 |
| ----------------------- | ------------------------------------------- |
| `-s, --source <SOURCE>` | Session source                              |
| `--max-size <SIZE>`     | Maximum stored size per session             |
| `-y, --yes`             | Skip confirmation prompt                    |

## dataset list

List datasets.

```cli
gage dataset list [OPTIONS]
```

The listing shows each dataset's session and attachment counts, tags, and
origin. A dataset a scan created records that scan as its origin.

| Option                | Description                                   |
| --------------------- | --------------------------------------------- |
| `-m, --more`          | Show more items. Repeat to show more per use. |
| `-a, --all`           | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |
| `--full-id`           | Show the full dataset ID                      |

## dataset show

Show a dataset: its tags, sessions, attachments, and the scans that ran over
it.

```cli
gage dataset show <DATASET>
```

## dataset delete

Delete datasets.

```cli
gage dataset delete [OPTIONS] <IDS>...
```

Member sessions and the scan runs that used the dataset are kept unless
`--sessions` is given.

| Option        | Description                                                                       |
| ------------- | --------------------------------------------------------------------------------- |
| `--sessions`  | Also remove member sessions from the store. A session another dataset holds is kept. |
| `-y, --yes`   | Skip confirmation prompt                                                          |
