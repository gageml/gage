---
title: gage fetch
description:
  Command reference for gage fetch, which adopts objects from a remote or URL
  into the store
---

The `gage fetch` command adopts objects from a remote into the store. It fetches
the named objects and what they link, or the whole remote with `--all`, and
reports what could not be merged.

```cli
gage fetch [OPTIONS] <REPOSITORY> [OBJECTS]...
```

`REPOSITORY` is a remote name from
[`gage remote add`](/docs/commands/remote), or any Git URL or path. A URL is
fetched without adding a remote, and nothing of it stays in the store. Objects
are tag names, object IDs, or unique ID prefixes as the remote has them. With
no objects, `--all` is required.

| Option      | Description                                                        |
| ----------- | ------------------------------------------------------------------ |
| `--all`     | Fetch every object and tag from the remote. The fetch previews the counts and asks to continue. |
| `--ours`    | Keep the local side on conflict                                    |
| `--theirs`  | Take the remote side on conflict                                   |
| `--list`    | List what the remote has without fetching                          |
| `-y, --yes` | Skip confirmation prompt                                           |

`--list` prints every tag the remote advertises with how the local store
compares: new, same, or differs. Objects follow as a count by state. Nothing is
transferred or written.

To copy data in the other direction, see [`gage push`](/docs/commands/push).
