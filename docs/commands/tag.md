---
title: gage tag
description:
  Command reference for gage tag, including subcommands to add, list, and
  delete tags
---

The `gage tag` command manages tags. A tag is a name for a stored object of any
type. Any argument that takes an object ID accepts a tag name in its place.

```cli
gage tag <COMMAND>
```

## tag add

Add a tag.

```cli
gage tag add [OPTIONS] <NAME> <OBJECT>
```

`NAME` follows Git ref naming rules. `/` is allowed, so names can be grouped,
as in `garrett/baseline`. `OBJECT` is a tag name, an object ID, or a unique ID
prefix.

| Option        | Description              |
| ------------- | ------------------------ |
| `-f, --force` | Move the tag if it exists |
| `-y, --yes`   | Skip confirmation prompt |

`gage scan --tag` and `gage dataset add --tag` tag a new object as it is
created.

## tag list

List tags.

```cli
gage tag list [OPTIONS]
```

| Option                | Description                                   |
| --------------------- | --------------------------------------------- |
| `--orphaned`          | Show tags whose object is deleted             |
| `-m, --more`          | Show more items. Repeat to show more per use. |
| `-a, --all`           | Show all items                                |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20) |
| `--full-id`           | Show the full object ID                       |

## tag delete

Delete tags. The objects they named are not changed.

```cli
gage tag delete [OPTIONS] <NAMES>...
```

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
