---
title: gage resolve
description:
  Command reference for gage resolve, which starts a Claude Code session to
  resolve issues
---

The `gage resolve` command resolves issues in a Claude Code session. It starts
an interactive session that resolves pending issues, then walks through open
issues as directed. For background, see [Issues](/docs/issues).

```cli
gage resolve [OPTIONS] [IDS]... [-- CLAUDE_ARGS...]
```

Issue IDs (or prefixes) scope the session. If omitted, all pending and open
issues are in scope. Arguments after `--` pass through to `claude`.

| Option            | Description           |
| ----------------- | --------------------- |
| `--model <MODEL>` | Model for the session |
