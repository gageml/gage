---
title: gage push
description:
  Command reference for gage push, which sends stored objects to a remote
---

The `gage push` command sends objects to a remote. Everything an object links
goes with it: a scan's dataset, sessions, notes, issues, and agent sessions.
The whole store is sent only by [`gage backup`](/docs/commands/backup).

```cli
gage push [OPTIONS] <REPOSITORY> <OBJECTS>...
```

`REPOSITORY` is a remote name from
[`gage remote add`](/docs/commands/remote), or any Git URL or path. Objects are
tag names, object IDs, or unique ID prefixes.

| Option         | Description                                    |
| -------------- | ---------------------------------------------- |
| `--with-notes` | Also push notes that target the named objects  |
| `-y, --yes`    | Skip confirmation prompt                       |

To copy data in the other direction, see [`gage fetch`](/docs/commands/fetch).
