---
title: gage remote
description:
  Command reference for gage remote, including subcommands to add, list, and
  delete remotes
---

The `gage remote` command manages remotes. A remote is a Git repository the
store pushes to, fetches from, or backs up to. See
[`gage push`](/docs/commands/push), [`gage fetch`](/docs/commands/fetch), and
[`gage backup`](/docs/commands/backup).

```cli
gage remote <COMMAND>
```

## remote add

Add a remote.

```cli
gage remote add [OPTIONS] <NAME> <URL>
```

`URL` is any Git URL or path. A project's own repository is a valid remote.
What Gage stores there is not transferred by a clone or seen by a checkout.

| Option      | Description                                                                    |
| ----------- | ------------------------------------------------------------------------------ |
| `--backup`  | Mark the remote as a backup target. `gage backup` sends the whole store to a backup target and refuses any other remote. |
| `-y, --yes` | Skip confirmation prompt                                                       |

## remote list

List remotes.

```cli
gage remote list
```

## remote delete

Delete remotes. The remote and what it tracked are removed locally. Nothing on
the remote is changed.

```cli
gage remote delete [OPTIONS] <NAMES>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
