---
title: gage note
description:
  Command reference for gage note, including subcommands to list, add, show,
  edit, and delete notes
---

The `gage note` command manages notes. A note is a value a scanner or person
assigns to a session, session lines, attachment, or other stored object. For
background, see [Notes](/docs/notes).

```cli
gage note <COMMAND>
```

Commands that take note IDs accept unique ID prefixes. A text argument accepts
`@PATH` to read the text from a file. Text cannot begin with a literal `@`.

## note list

List notes.

```cli
gage note list [OPTIONS]
```

| Option                | Description                                   |
| --------------------- | --------------------------------------------- |
| `--name <NAME>`       | Show notes with this name                     |
| `-m, --more`          | Show more items. Repeat to show more per use. |
| `-a, --all`           | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |
| `--full-id`           | Show the full note ID                         |

## note add

Add a note of your own.

```cli
gage note add [OPTIONS] [TEXT]
```

The command prompts for the text when you omit it.

| Option                    | Description                             |
| ------------------------- | --------------------------------------- |
| `-t, --target <TARGET>`   | Note target (ID or prefix)              |
| `-n, --name <NAME>`       | Note name (default: `comment`)          |
| `--json`                  | Store the text as JSON                  |
| `-m, --metadata <JSON>`   | Note metadata as a JSON object          |
| `-u, --user <USER>`       | Author username (default: `$USER`)      |
| `-y, --yes`               | Skip prompts. Requires `TEXT`.          |

A target is an object ID or unique prefix, optionally followed by `#` and a
line selection such as `12`, `12-20`, or `12-20,31`. A line selection applies
to a session and limits the match to sessions.

Names need not be unique; writing the same name for the same target adds
another note. See [Authors](/docs/notes#authors).

## note show

Show a note.

```cli
gage note show [OPTIONS] <ID>
```

| Option       | Description    |
| ------------ | -------------- |
| `-d, --doc`  | Show note docs |

## note edit

Edit a note.

```cli
gage note edit [OPTIONS] <ID> [TEXT]
```

The command prompts for new text when no argument or option is given.

| Option                  | Description                                                 |
| ----------------------- | ----------------------------------------------------------- |
| `-t, --target <TARGET>` | New note target (ID or prefix, with optional `#` line selection) |
| `-n, --name <NAME>`     | New note name                                               |
| `--json`                | Store the text as JSON                                      |
| `-y, --yes`             | Skip prompts. Requires `TEXT`, `--name`, or `--target`.     |

## note delete

Delete notes.

```cli
gage note delete [OPTIONS] <IDS>...
```

In general this is not needed. Scanners benefit from keeping the note record
across scans.

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
