---
title: gage backup
description:
  Command reference for gage backup, which sends the whole store to a backup
  remote
---

The `gage backup` command backs up the store. It adds every session from each
configured source to the store, then sends every object, tag, and key to a
remote added with `gage remote add --backup`. Restoring is
[`gage pull`](/docs/commands/pull).

```cli
gage backup [OPTIONS] [REPOSITORY]
```

`REPOSITORY` is a backup remote, or any Git URL or path. It is optional when
exactly one backup remote exists.

| Option                  | Description                                                        |
| ----------------------- | ------------------------------------------------------------------ |
| `-N, --native <SOURCE>`     | Add sessions from this source before the backup (repeatable). Replaces the configured `backup.sources` list for this run. |
| `-S, --stored`          | Back up stored sessions only. No source is read first.            |
| `--force`               | Replace diverged objects and tags on the remote                    |
| `--prune`               | Delete objects and tags that are only on the remote                |
| `-y, --yes`             | Skip confirmation prompt                                           |

A source is `SCHEME:PATH`, as described in
[`gage session`](/docs/commands/session). The sources read by default are the
`backup.sources` setting; see [`gage config`](/docs/commands/config#settings).

A diverged object is one the remote holds with a history the store's copy does
not continue, as every object is after a store is deleted and rebuilt from its
sources. Without `--force`, diverged objects are listed, left alone, and
reported as an error once the rest is sent.

An object that links a session fetched from a remote and not merged is
withheld, as is a tag that names it. A backup leaves withheld objects and tags
out and reports their count. See
[`gage scan --stored-remote`](/docs/commands/scan).

`--prune` deletes the objects and tags that the remote holds and the store does
not. An object deleted in the store is kept as a deletion record and is backed
up, not pruned. Without `--prune`, such objects and tags are listed and left
alone.
