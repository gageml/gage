---
title: gage backup
description:
  Command reference for gage backup, which sends the whole store to a backup
  remote
---

The `gage backup` command backs up the store. It adds every session from each
configured source to the store, then sends every object, tag, and key to a
remote added with `gage remote add --backup`. Restoring is
[`gage fetch`](/docs/commands/fetch).

```cli
gage backup [OPTIONS] [REPOSITORY]
```

`REPOSITORY` is a backup remote, or any Git URL or path. It is optional when
exactly one backup remote exists.

| Option                  | Description                                                        |
| ----------------------- | ------------------------------------------------------------------ |
| `-s, --source <SOURCE>` | Add sessions from this source before the backup (repeatable). Replaces the configured `backup.sources` list for this run. |
| `-S, --stored`          | Back up stored sessions only. No source is read first.            |
| `-y, --yes`             | Skip confirmation prompt                                           |

A source is `SCHEME:PATH`, as described in
[`gage session`](/docs/commands/session). The sources read by default are the
`backup.sources` setting; see [`gage config`](/docs/commands/config#settings).
