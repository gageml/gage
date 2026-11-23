---
title: gage status
description:
  Command reference for gage status, which shows each driver's agent, login,
  and plugin, the session corpus, and storage status
---

The `gage status` command shows the state of a Gage installation: for each
driver, the coding agent it works with, its login, and whether the Gage plugin
is registered; the available session corpus; and storage use under `~/.gage`.

An agent that is not installed is shown as not installed. It is reported as a
problem only when its driver is the default driver.

```cli
gage status [OPTIONS]
```

| Option            | Description                                        |
| ----------------- | -------------------------------------------------- |
| `-v, --verbose`   | Show per-subdirectory storage breakdown under `~/.gage` |

For the state of the store itself, see
[`gage store status`](/commands/store#store-status).
