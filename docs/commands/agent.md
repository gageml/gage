---
title: gage agent
description:
  Command reference for gage agent, which runs a scanner agent directly for
  scanner development
---

:::note

This command supports scanner development. Normal use does not require it.
Scans run agents for you.

:::

The `gage agent` command runs a scanner agent.

```cli
gage agent [OPTIONS] [AGENT] [SESSION]...
```

The agent is named as `<scanner>::<fn>` or as a bare `<fn>`. A bare fn name
must match exactly one declared agent. Session IDs (or prefixes) scope the
agent to those sessions only. If omitted, all available sessions are available
to the agent.

| Option              | Description                                        |
| ------------------- | -------------------------------------------------- |
| `-n, --limit <N>`   | Scope to the latest N sessions                     |
| `-d, --days <N>`    | Scope to sessions modified in past N days (default 30) |
| `-a, --all`         | Scope to all sessions                              |
| `-i, --interactive` | Run interactively in a Claude Code session         |
| `-y, --yes`         | Skip confirmation prompt                           |
| `-l, --list`        | List available agents                              |
