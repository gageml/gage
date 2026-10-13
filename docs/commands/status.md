---
title: gage status
description:
  Command reference for gage status, which shows Claude Code login, plugin,
  session corpus, and storage status
---

The `gage status` command shows the state of a Gage installation: the Claude
Code login, whether the Gage plugin is registered, the available session
corpus, and storage use under `~/.gage`.

```cli
gage status [OPTIONS]
```

| Option            | Description                                        |
| ----------------- | -------------------------------------------------- |
| `-v, --verbose`   | Show per-subdirectory storage breakdown under `~/.gage` |

For the state of the store itself, see
[`gage store status`](/docs/commands/store#store-status).
