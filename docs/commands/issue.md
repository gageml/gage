---
title: gage issue
description:
  Command reference for gage issue, including subcommands to list, show, add,
  close, open, comment on, and delete issues
---

The `gage issue` command manages issues. An issue is a finding reported by a
scanner or added by you. For background, see [Issues](/docs/issues).

```cli
gage issue <COMMAND>
```

Commands that take issue IDs accept unique ID prefixes. Commands that modify
issues prompt for confirmation. Use `--yes` to skip the prompt.

Options that take text accept `@PATH` to read the text from a file. Text
cannot begin with a literal `@`.

## issue list

List issues.

```cli
gage issue list [OPTIONS]
```

The command shows open issues, newest first, up to a default limit of 20.

| Option                | Description                                   |
| --------------------- | --------------------------------------------- |
| `--name <NAME>`       | Filter by issue name                          |
| `-c, --closed`        | Show closed issues                            |
| `-m, --more`          | Show more items. Repeat to show more per use. |
| `-a, --all`           | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |
| `--full-id`           | Show the full issue ID                        |

## issue show

Show an issue.

```cli
gage issue show <ID>
```

The command prints the issue description, its supporting evidence, related
issues, and its event history. Events include status changes and comments.

## issue add

Add an issue of your own.

```cli
gage issue add [OPTIONS]
```

The command prompts for a title and description when you omit them.

| Option                        | Description                                       |
| ----------------------------- | ------------------------------------------------- |
| `-t, --title <TITLE>`         | Title (prompted if omitted)                       |
| `-d, --description <TEXT>`    | Description (prompted if omitted)                 |
| `-n, --name <NAME>`           | Issue name (default: `user-issue`)                |
| `-e, --evidence <NOTE>`       | Note cited as evidence (ID or prefix; repeatable) |
| `-u, --user <USER>`           | Author username (default: `$USER`)                |
| `-p, --pending`               | Add as _pending_ instead of the default _open_    |
| `-y, --yes`                   | Skip prompts. Requires `--title`.                 |

## issue close

Close one or more issues.

```cli
gage issue close [OPTIONS] <IDS>...
```

A close records a reason: _completed_ by default, _wontfix_ for a non-issue,
or _duplicate_ of another issue. The command prompts for an optional message.

| Option                    | Description                                          |
| ------------------------- | ---------------------------------------------------- |
| `-c, --completed`         | Close as _completed_                                 |
| `-w, --wontfix`           | Close as _wontfix_                                   |
| `-d, --duplicate <ISSUE>` | Close as _duplicate_ of another issue (ID or prefix) |
| `-m, --message <TEXT>`    | Message explaining the close                         |
| `-y, --yes`               | Skip prompts                                         |

## issue open

Open pending or closed issues.

```cli
gage issue open [OPTIONS] <IDS>...
```

| Option                 | Description                 |
| ---------------------- | --------------------------- |
| `-m, --message <TEXT>` | Message explaining the open |
| `-y, --yes`            | Skip prompts                |

## issue comment

Comment on issues.

```cli
gage issue comment [OPTIONS] <IDS>...
```

The command prompts for the comment text when you omit `--message`. Comments
appear in the issue's event history in `gage issue show`.

| Option                 | Description                                 |
| ---------------------- | ------------------------------------------- |
| `-m, --message <TEXT>` | Comment text (prompted if omitted)          |
| `-y, --yes`            | Skip confirmation prompt. Requires `--message`. |

## issue delete

Delete issues.

```cli
gage issue delete [OPTIONS] <IDS>...
```

A delete removes the issue record. To dismiss a finding while keeping its
record, close the issue instead.

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
