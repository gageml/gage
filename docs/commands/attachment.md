---
title: gage attachment
description:
  Command reference for gage attachment, including subcommands to add, list,
  show, and remove attachments
---

The `gage attachment` command manages attachments. An attachment is a set of
files added from a directory for scanners to read, such as a Claude Code config
directory. It may be named, and it may target one or more stored objects such
as sessions. Datasets hold attachments the way they hold sessions.

```cli
gage attachment <COMMAND>
```

Commands that take attachments accept a name, an ID, or a unique ID prefix.

## attachment add

Add an attachment from a directory.

```cli
gage attachment add [OPTIONS] [INCLUDE]...
```

Files are selected by path globs relative to the root, as in a shell.
`settings.json` is the one file at the root, `**/settings.json` is every file
by that name, `*` and `?` do not cross `/`, and `{a,b}` and `[a-z]` are
supported. Excludes use the same form, and a directory an exclude matches is
not entered. At least one include is required; `**/*` selects every file. An
attachment holds at most 10 MiB and 1000 files.

| Option                      | Description                                                       |
| --------------------------- | ----------------------------------------------------------------- |
| `-r, --root <DIR>`          | Directory to select files from (default: current directory). Stored paths are relative to it. |
| `-e, --exclude <PATTERN>`   | Files to exclude (glob; repeatable)                               |
| `-S, --stored <ATTACHMENT>` | Add an attachment already in the store to a dataset. Requires `--dataset`. |
| `-s, --scanner <SCANNER>`   | Attach the files a scanner's scan reads (name or path; repeatable). Requires `--dataset`. |
| `-d, --dataset <DATASET>`   | Store the attachment and add it to a dataset in one step          |
| `-t, --target <OBJECT>`     | Target object such as a session or note (ID or prefix; repeatable) |
| `-n, --name <NAME>`         | Attachment name. Scanners select attachments by name. A name is letters, digits, `-`, `_`, and `.`. |
| `-k, --key <KEY>`           | Attachment key. A later add under the same key updates this attachment. |
| `-y, --yes`                 | Skip confirmation prompt                                          |

Adding again under the same key updates the attachment, with any new targets
added to it. A named add without `--key` is keyed by its name, root, and
patterns. An unnamed add without `--key` always creates a new attachment.

`--scanner` prepares an existing dataset for a scan: each scanner attaches the
files its tasks read. A scan runs these attach tasks itself on a dataset it
creates, but not on a dataset given with `gage scan --dataset`.

## attachment list

List attachments.

```cli
gage attachment list [OPTIONS]
```

| Option                    | Description                                   |
| ------------------------- | --------------------------------------------- |
| `-d, --dataset <DATASET>` | List attachments in a dataset, in dataset order |
| `-m, --more`              | Show more items. Repeat to show more per use. |
| `-a, --all`               | Show all items                                |
| `-n, --limit <LIMIT>`     | Limit the number of items shown (default: 20) |
| `--full-id`               | Show the full attachment ID                   |

## attachment show

Show an attachment and its files.

```cli
gage attachment show <ATTACHMENT>
```

## attachment remove

Remove attachments from the store or from a dataset.

```cli
gage attachment remove [OPTIONS] <ATTACHMENTS>...
```

Without `--dataset`, each attachment is removed from every dataset that holds
it and marked removed in the store. With `--dataset`, each attachment is
removed from that dataset only and stays in the store.

| Option                    | Description                                   |
| ------------------------- | --------------------------------------------- |
| `-d, --dataset <DATASET>` | Remove from this dataset only (ID or prefix)  |
| `-y, --yes`               | Skip confirmation prompt                      |
