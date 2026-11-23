---
title: gage backup
description:
  Command reference for gage backup, which sends the whole store to a backup
  remote
---

The `gage backup` command backs up the store. It adds every session from each
configured native source to the store, then sends every object, tag, and key to
a remote added with `gage remote add --backup`. Restoring is
[`gage pull`](/commands/pull).

```cli
gage backup [OPTIONS] [REPOSITORY]
```

`REPOSITORY` is a backup remote, or any Git URL or path. It is optional when
exactly one backup remote exists.

| Option                  | Description                                                        |
| ----------------------- | ------------------------------------------------------------------ |
| `-N, --native <SOURCE>`     | Add sessions from this source before the backup (repeatable). Replaces the configured `backup.native` list for this run. |
| `-S, --stored`          | Back up stored sessions only. No source is read first.            |
| `--force`               | Replace diverged objects and tags on the remote                    |
| `--prune`               | Delete objects and tags that are only on the remote                |
| `-y, --yes`             | Skip confirmation prompt                                           |

A source is `SCHEME:PATH` or a configured name, as described in
[`gage session`](/commands/session). The sources read by default are the
`backup.native` setting; see [`gage config`](/commands/config#settings).

A diverged object is one the remote holds with a history the store's copy does
not continue, as every object is after a store is deleted and rebuilt from its
sources. Without `--force`, diverged objects are listed, left alone, and
reported as an error once the rest is sent.

An object the remote holds a newer edit of is not diverged. It happens when
another store backs up to the same remote after a session there has grown. The
backup counts such objects as newer on the remote and never sends or replaces
them, with `--force` or without. The store must hold the newer edit to
recognize it, as it does after a `gage fetch` from the store that wrote it.
Otherwise the object is reported as diverged.

An object that links a session fetched from a remote and not merged is
withheld, as is a tag that names it. A backup leaves withheld objects and tags
out and reports their count. See
[`gage scan --stored-remote`](/commands/scan).

`--prune` deletes the objects and tags that the remote holds and the store does
not. An object deleted in the store is kept as a deletion record and is backed
up, not pruned. Without `--prune`, such objects and tags are left alone and
are not reported. A remote that several stores back up to holds objects that
each store lacks.
