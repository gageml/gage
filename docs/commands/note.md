---
title: gage note
description:
  Command reference for gage note, including subcommands to list, show, add,
  edit, and delete notes
---

The `gage note` command manages notes. A note is a value a scanner or person
assigns to a session, session line, or other topic. For background, see
[Notes](/docs/notes).

```cli
gage note <COMMAND>
```

Commands that take note IDs accept unique ID prefixes.

## note list

List notes.

```cli
gage note list [OPTIONS]
```

| Option                | Description                                   |
| --------------------- | --------------------------------------------- |
| `-m, --more`          | Show more items. Repeat to show more per use. |
| `-a, --all`           | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |
| `--session <SESSION>` | Filter by target session ID (or prefix)       |
| `--name <NAME>`       | Filter by note name                           |

## note show

Show a note.

```cli
gage note show [OPTIONS] <ID>
```

| Option         | Description         |
| -------------- | ------------------- |
| `-t, --target` | Show target content |
| `-d, --doc`    | Show note docs      |

## note add

Add a note of your own.

```cli
gage note add [OPTIONS]
```

The command prompts for the value when you omit `--value`.

| Option                | Description                                            |
| --------------------- | ------------------------------------------------------ |
| `-t, --target <TARGET>` | Target session, as a full session ID. Append `:LINE` to specify a session line number. |
| `-n, --name <NAME>`   | Note name (default: `comment`)                         |
| `-v, --value <VALUE>` | Note value (prompted if omitted)                       |
| `-u, --user <USER>`   | Author username (default: `$USER`)                     |

Names need not be unique; writing the same name for the same target adds
another note. See
[Authors and duplicate policies](/docs/notes#authors-and-duplicate-policies).

## note edit

Edit a note.

```cli
gage note edit [OPTIONS] <ID>
```

| Option                | Description                      |
| --------------------- | -------------------------------- |
| `-v, --value <VALUE>` | New value (prompted if omitted)  |

## note delete

Delete notes.

```cli
gage note delete [OPTIONS] [IDS]...
```

In general this is not needed. Scanners benefit from keeping the note record
across scans.

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
