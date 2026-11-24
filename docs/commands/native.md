---
title: gage native
description:
  Command reference for gage native, including subcommands to add, list, edit,
  rename, and delete native sources
---

The `gage native` command manages native sources. A native source is a
directory of coding agent sessions. A name given to one is accepted wherever a
session source is: the `--native` option of every command, the `backup.native`
setting, and the `native` parameter of the `query` tool. The names are the
`native` table of the config files. See
[Native source names](/commands/config#native-source-names).

```cli
gage native <COMMAND>
```

A `SOURCE` is `SCHEME:PATH`, or `SCHEME` alone for the default directory of
that scheme. The schemes are `claude`, `codex`, and `pi`.

Each subcommand that writes a config file keeps the rest of the file's text as
written.

## native add

Add a native source. The name `default` is the source a command reads when it
is given none.

```cli
gage native add [OPTIONS] <NAME> <SOURCE>
```

| Option      | Description                                                 |
| ----------- | ----------------------------------------------------------- |
| `--project` | Write the project config file in place of the user config file |
| `-y, --yes` | Skip confirmation prompt                                    |

Without `--project` the name is written to the user config file,
`~/.gage/config.toml`, and `SOURCE` must not have a relative path.

With `--project` the name is written to `.gage/config.toml` under the nearest
directory, from the current one up, that has a `.gage` directory. When no
directory has one, the file is written under the current directory. The name is
in effect for commands run in that directory and below, where it replaces the
same name in the user config file. A relative path in `SOURCE` is under the
project directory.

A name that a file already sets is an error for that file. Use
`gage native edit` to change its source.

## native list

List native sources. Each row is a name in effect in the current directory, its
source, and the config file that sets it.

```cli
gage native list
```

The first row is `default`. When no config file sets it, the row shows the
default directory of the default driver and `drivers.default` in place of a
file. The rows that follow are the schemes, which show `built in` unless a
config file gives a source the same name. The remaining rows are the other
names.

## native edit

Edit a native source. The source of `NAME` is replaced in the config file that
sets it for the current directory.

```cli
gage native edit [OPTIONS] <NAME> <SOURCE>
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

## native rename

Rename a native source. The source keeps its value and its config file. When
the `backup.native` setting lists the name and no other source has it, the
setting is changed to list the new name.

```cli
gage native rename [OPTIONS] <NAME> <NEW_NAME>
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

## native delete

Delete native sources. Each name is removed from the config file that sets it
for the current directory. No session is deleted.

```cli
gage native delete [OPTIONS] <NAMES>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |

A name that the `backup.native` setting lists is not deleted while no other
source has that name. Remove it from the setting first.
