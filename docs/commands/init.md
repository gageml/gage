---
title: gage init
description:
  Command reference for gage init, which sets up Gage and registers the Claude
  Code plugin
---

The `gage init` command sets up Gage. It registers the Gage plugin with Claude
Code.

```cli
gage init [OPTIONS]
```

| Option                         | Description                                   |
| ------------------------------ | --------------------------------------------- |
| `-r, --remove`                 | Uninstall Gage from Claude Code               |
| `-y, --yes`                    | Skip confirmation prompt                      |
| `--import-data <PATH>`         | Import data from PATH                         |
| `--import-data-preview <PATH>` | Preview import without modifying the database |

An import never overwrites rows with the same IDs. Rejected rows are written to
a JSON file next to the modified database file at `~/.gage/data/gage.db`.

To remove the rest of a Gage installation, see
[_Uninstall Gage_](/docs/uninstall).
