---
title: gage merge
description:
  Command reference for gage merge, which merges objects fetched from a remote
  into the store
---

The `gage merge` command merges objects that
[`gage fetch`](/commands/fetch) fetched from a remote into the store. It
merges the named objects and what they link, or everything fetched from the
remote with `--all`, and reports what could not be merged. The remote is not
contacted.

```cli
gage merge [OPTIONS] <REMOTE> [OBJECTS]...
```

`REMOTE` is a remote name from [`gage remote add`](/commands/remote).
Objects are tag names, object IDs, or unique ID prefixes as the fetch of the
remote has them. With no objects, the command merges the objects the remote
records as its fetch default. Pass `--all` to merge everything fetched from the
remote. A remote that has not been fetched is an error that names `gage fetch`.

| Option      | Description                                                        |
| ----------- | ------------------------------------------------------------------ |
| `-a, --all` | Merge every fetched object and tag. The merge previews the counts and asks to continue. |
| `--ours`    | Keep the local side on conflict                                    |
| `--theirs`  | Take the remote side on conflict                                   |
| `-y, --yes` | Skip confirmation prompt                                           |

An object the store does not have is created. An object changed on one side
alone takes the newer commit. An object changed on both sides is merged where
its type allows it. One that cannot be merged is reported as a conflict and left
as it is, and the command exits with status 1. Run the command again with the
conflicting objects named and `--ours` or `--theirs` to resolve it.

To fetch and merge in one run, see [`gage pull`](/commands/pull).
