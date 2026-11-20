---
title: gage index
description:
  Command reference for gage index, which updates or rebuilds the Gage session
  index
---

:::note

This command supports Gage development and troubleshooting. Normal use does not
require it. Gage maintains the index automatically.

:::

The `gage index` command updates the Gage session index for a session source.

```cli
gage index [OPTIONS]
```

| Option                  | Description                                  |
| ----------------------- | -------------------------------------------- |
| `-N, --native <SOURCE>`     | Session source whose index to maintain       |
| `-S, --stored`          | Maintain the index of stored sessions        |
| `-R, --stored-remote <NAME>` | Maintain the index of sessions fetched from a remote |
| `--rebuild`             | Rebuild the index                            |
| `--status`              | Show index status                            |
| `--list`                | List every index under the cache directory   |
| `--remove <CORPUS>`     | Remove the indexes of a corpus               |

A source is `SCHEME:PATH` or a configured name, as described in
[`gage session`](/docs/commands/session). The default is the Claude Code home
directory.

`--list` shows one row per corpus: the corpus path, the indexes it holds, the
source it was built from, whether that source is still present, and its size.
An entry from an earlier cache layout is listed as legacy. `--remove` takes a
corpus path as `--list` shows it, such as `store/1a2b3c4d5e6f7a8b`, and removes
its whole directory, whether or not its source is still present.

The object index of the Gage store is a separate index. See
[`gage store index`](/docs/commands/store#store-index).
