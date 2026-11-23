---
title: gage pull
description:
  Command reference for gage pull, which fetches objects from a remote or URL
  and merges them into the store
---

The `gage pull` command fetches objects from a remote and merges them into the
store. It does what [`gage fetch`](/commands/fetch) followed by
[`gage merge`](/commands/merge) does, in one run. It pulls the named
objects and what they link, or the whole remote with `--all`, and reports what
could not be merged.

```cli
gage pull [OPTIONS] <REPOSITORY> [OBJECTS]...
```

`REPOSITORY` is a remote name from
[`gage remote add`](/commands/remote), or any Git URL or path. A URL is
pulled without adding a remote, and nothing of it stays in the store but the
merged objects. Objects are tag names, object IDs, or unique ID prefixes as the
remote has them. With no objects, the command pulls the objects the remote
records as its fetch default. Pass `--all` to pull the whole remote.

| Option      | Description                                                        |
| ----------- | ------------------------------------------------------------------ |
| `-a, --all` | Pull every object and tag from the remote. The pull previews the counts and asks to continue. |
| `--ours`    | Keep the local side on conflict                                    |
| `--theirs`  | Take the remote side on conflict                                   |
| `-y, --yes` | Skip confirmation prompt                                           |

An object that cannot be merged is reported as a conflict and left as it is,
and the command exits with status 1. To resolve a conflict from a remote, run
[`gage merge`](/commands/merge) with the conflicting objects named and
`--ours` or `--theirs`. To resolve one from a URL, run `gage pull` again the
same way.

To copy data in the other direction, see [`gage push`](/commands/push).
