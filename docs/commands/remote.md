---
title: gage remote
description:
  Command reference for gage remote, including subcommands to add, list, edit,
  rename, and delete remotes
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

## remote edit

Edit a remote. Each option changes what the remote recorded for it. What no
option names is left as it is. At least one option is required.

```cli
gage remote edit [OPTIONS] <NAME>
```

| Option                    | Description                                                              |
| ------------------------- | ------------------------------------------------------------------------ |
| `--url <URL>`             | Replace the remote URL                                                   |
| `--backup`                | Mark the remote as a backup target                                       |
| `--no-backup`             | Clear the backup target mark                                             |
| `--push <OBJECT>`         | Replace the objects `gage push` sends when no object is named (repeatable) |
| `--add-push <OBJECT>`     | Add an object to push (repeatable)                                       |
| `--remove-push <OBJECT>`  | Remove an object to push (repeatable)                                    |
| `--no-push`               | Clear the objects to push                                                |
| `--fetch <OBJECT>`        | Replace the objects `gage fetch` takes when no object is named (repeatable) |
| `--add-fetch <OBJECT>`    | Add an object to fetch (repeatable)                                      |
| `--remove-fetch <OBJECT>` | Remove an object to fetch (repeatable)                                   |
| `--no-fetch`              | Clear the objects to fetch                                               |
| `-y, --yes`               | Skip confirmation prompt                                                 |

An `OBJECT` is a tag, an ID, or an ID prefix. `--push` cannot be combined with
`--add-push`, `--remove-push`, or `--no-push`, and `--fetch` follows the same
rule. Removing an object the remote does not record is an error.

## remote rename

Rename a remote. The remote keeps its URL, its settings, and what it tracked.
Nothing on the remote is changed.

```cli
gage remote rename [OPTIONS] <NAME> <NEW_NAME>
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

## remote delete

Delete remotes. The remote and what it tracked are removed locally. Nothing on
the remote is changed.

```cli
gage remote delete [OPTIONS] <NAMES>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
