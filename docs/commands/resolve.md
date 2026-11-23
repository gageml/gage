---
title: gage resolve
description:
  Command reference for gage resolve, which starts a coding agent session to
  resolve issues
---

The `gage resolve` command resolves issues in a session of the default driver's
coding agent: Claude Code, Codex, or Pi. It starts an interactive session that
resolves pending issues, then walks through open issues as directed. For
background, see [Issues](/issues).

A session in Pi cannot assign an issue. See
[Known issues](https://github.com/gageml/gage/blob/main/KNOWN_ISSUES.md#pi).

```cli
gage resolve [OPTIONS] [IDS]... [-- AGENT_ARGS...]
```

Issue IDs (or prefixes) scope the session. If omitted, all pending and open
issues are in scope. Arguments after `--` pass through to the agent's command:
`claude`, `codex`, or `pi`.

| Option                | Description                                        |
| --------------------- | -------------------------------------------------- |
| `-m, --model <MODEL>` | Model for the session (prompted if omitted)        |
| `-y, --yes`           | Skip confirmation prompt                           |
