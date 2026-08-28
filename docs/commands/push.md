---
title: gage push
description:
  Command reference for gage push, which copies Gage data to configured remotes
  or a local directory
---

The `gage push` command copies Gage data to remotes.

```cli
gage push <--list-remotes|--remote NAME|--all|--target DIR>
```

Exactly one selection option is required. `--remote`, `--all`, and `--target`
are mutually exclusive.

| Option              | Description                                             |
| ------------------- | ------------------------------------------------------- |
| `--list-remotes`    | List configured remotes                                 |
| `-r, --remote <NAME>` | Push to a named remote. Repeat to push to several.    |
| `-a, --all`         | Push Gage data to all remotes                           |
| `-t, --target <DIR>` | Push Gage data to a local directory                    |

To copy data in the other direction, see [`gage pull`](/docs/commands/pull).
