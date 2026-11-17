---
title: gage fetch
description:
  Command reference for gage fetch, which fetches objects from a remote without
  merging them into the store
---

The `gage fetch` command fetches objects from a remote and keeps them apart
from the objects of the store. It fetches the named objects and what they link,
or the whole remote with `--all`. The store's own objects do not change until
[`gage merge`](/docs/commands/merge) merges what was fetched.

```cli
gage fetch [OPTIONS] <REMOTE> [OBJECTS]...
```

`REMOTE` is a remote name from [`gage remote add`](/docs/commands/remote). A Git
URL or path is not accepted, because a fetch is kept under the remote's name.
Use [`gage pull`](/docs/commands/pull) to fetch and merge a URL in one run.
Objects are tag names, object IDs, or unique ID prefixes as the remote has them.
With no objects, the command fetches the objects the remote records as its fetch
default. Pass `--all` to fetch the whole remote.

| Option      | Description                                                        |
| ----------- | ------------------------------------------------------------------ |
| `-a, --all` | Fetch every object and tag from the remote. The fetch previews the counts and asks to continue. |
| `--list`    | List what the remote has without fetching                          |
| `-y, --yes` | Skip confirmation prompt                                           |

`--list` prints every tag the remote advertises with how the local store
compares: new, same, or differs. Objects follow as a count by state. Nothing is
transferred or written. `--list` accepts a URL or path as well as a remote name.

A fetch ends by printing the `gage merge` command that merges what it fetched.

To copy data in the other direction, see [`gage push`](/docs/commands/push).
