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
| `-s, --source <SOURCE>` | Session source whose index to maintain       |
| `--rebuild`             | Rebuild the index                            |
| `--status`              | Show index status                            |

A source is `SCHEME:PATH`, as described in
[`gage session`](/docs/commands/session). The default is the Claude Code home
directory.

The object index of the Gage store is a separate index. See
[`gage store index`](/docs/commands/store#store-index).
