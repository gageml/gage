---
title: gage pull
description:
  Command reference for gage pull, which copies Gage data from configured
  remotes or a local directory
---

The `gage pull` command copies Gage data from remotes.

```cli
gage pull [OPTIONS] <--list-remotes|NAME|--source DIR>
```

The source is a named remote or a local directory. `NAME` and `--source` are
mutually exclusive. A local source directory uses the same layout as any other
remote.

| Option              | Description                                  |
| ------------------- | -------------------------------------------- |
| `--list-remotes`    | List configured remotes                      |
| `-s, --source <DIR>` | Pull from a local directory                 |
| `-t, --target <DIR>` | Destination directory (default `~/.gage-pull`) |

To copy data in the other direction, see [`gage push`](/docs/commands/push).
