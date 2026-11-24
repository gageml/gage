---
title: gage store
description:
  Command reference for gage store, including subcommands to create, inspect,
  and maintain the Gage store
---

:::note

Most of these commands support Gage development and troubleshooting. Normal
use does not require them. Sessions, datasets, notes, issues, and scans have
their own commands.

:::

The `gage store` command manages the Gage store. The store is a bare Git
repository at `~/.gage/store.git` that holds every Gage object. When
`GAGE_HOME` is set, the store is `$GAGE_HOME/store.git`.

A project can name a store of its own in its
[project config](/commands/config#project-config). Commands run in the project
directory use that store. When `GAGE_STORE` is set, the store is the
repository at that path, whatever a project config names.

```cli
gage store <COMMAND>
```

Commands that take an object accept a tag name, an object ID, or a unique ID
prefix.

## store init

Create the store. Running it on an existing store is harmless. In a project
whose config names a store, the command creates that store.

```cli
gage store init
```

## store status

Show store status.

```cli
gage store status [OPTIONS]
```

| Option    | Description                                    |
| --------- | ---------------------------------------------- |
| `--check` | Verify store integrity after showing status    |

## store index

Show object index status. The object index is a cache over the stored objects.
It is kept current on every open and is safe to delete at any time.

```cli
gage store index [OPTIONS]
```

| Option      | Description                                      |
| ----------- | ------------------------------------------------ |
| `--rebuild` | Delete the index and build it again from the store |

## store gc

Garbage collect the store by running `git gc`.

```cli
gage store gc [OPTIONS]
```

| Option             | Description                                                                 |
| ------------------ | --------------------------------------------------------------------------- |
| `--prune <EXPIRE>` | Prune unreachable objects older than this. Passed to `git gc --prune`. Common values are `now` and `2.weeks.ago`, the Git default. |

## store ls

List store paths.

```cli
gage store ls [PATHS]...
```

The store is one hierarchy: namespaces, the references in them, and the files
of the commit each reference points at. A path starts at the store root, runs
through a reference, and continues into the files of its commit. Without a
path, the command lists the Gage namespaces as a starting point.

A path with no wildcard lists the children of the container it names, or the
file it names. A path with a wildcard lists what it matches. `*` matches one
name and `**` matches any number of names, on either side of a reference.

## store cat

Print a stored file. The file's bytes are printed as stored.

```cli
gage store cat <PATH>
```

`PATH` is a path as `gage store ls` lists it. A wildcard is accepted when it
matches exactly one file.

## store delete

Delete objects of any type.

```cli
gage store delete [OPTIONS] <IDS>...
```

Each object is deleted as its own delete command would delete it, without that
command's cleanup: a dataset that links a deleted object keeps its link, and a
dataset's members are left in place. Use this on an object its own command
cannot read.

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

## store view

Browse stored objects in an interactive view. Objects are listed on the left.
The right side shows the selected object's versions, the objects it links, and
its files.

```cli
gage store view [OBJECT]
```

`OBJECT` selects an object on open.
