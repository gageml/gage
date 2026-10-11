---
title: gage query
description:
  Command reference for gage query, which queries stored sessions, notes,
  issues, and scans with SQL interactively or from the command line
---

The `gage query` command queries the Gage store with SQL. With no SQL argument,
it starts an interactive SQL prompt. With SQL arguments, it executes each
statement in order and exits.

```cli
gage query [OPTIONS] [SQL]...
```

An argument that names an existing file is read as a SQL script. Statements in
a script end with `;` and lines starting with `--` are comments.

| Option                  | Description                                       |
| ----------------------- | ------------------------------------------------- |
| `-f, --format <FORMAT>` | Output format: `table` (default), `csv`, `json`, `ndjson`, or `yaml` |
| `-q, --quiet`           | Suppress non-result output                        |
| `--timing`              | Enable query timings                              |
| `--stats`               | Enable query stats                                |
| `--system`              | Include system columns                            |
| `--scan <SCAN>`         | Query within a scan                               |

`--scan` takes a scan ID or unique prefix. The session tables then hold the
scan's sessions at the versions it read, and the scan tables hold only that
scan's tasks, notes, and issues.

`--system` adds the columns programs use to render or address a row, such as
`id_display` and `id_prefix`, which session tables hide by default.

Sessions that are not yet in the store are reachable through the
`native_session`, `native_message`, and `native_entry` table functions, which
take an optional source argument such as `'claude:<path>'`.

In the interactive prompt, `\d` lists tables, `\d <table>` shows a table's
columns, `\df` lists table functions, and `\?` shows all prompt commands.
