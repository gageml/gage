---
title: gage query
description:
  Command reference for gage query, which queries sessions with SQL
  interactively or from the command line
---

The `gage query` command queries sessions with SQL. Without `--command`, it
starts an interactive SQL prompt. With `--command`, it executes the given SQL
and exits.

```cli
gage query [OPTIONS]
```

| Option                  | Description                                       |
| ----------------------- | ------------------------------------------------- |
| `-A, --agent`           | Operate on agent sessions instead of Claude Code sessions |
| `-c, --command <COMMAND>` | Execute SQL and exit                            |
| `-f, --format <FORMAT>` | Output format: `table` (default), `csv`, `json`, `ndjson`, or `yaml` |
| `-q, --quiet`           | Suppress non-result output                        |
| `--timing`              | Enable query timings                              |
| `--stats`               | Enable query stats                                |
