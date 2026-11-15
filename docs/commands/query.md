---
title: gage query
description:
  Command reference for gage query, which queries sessions, notes, issues, and
  scans with SQL interactively or from the command line
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
| `-s, --source <SOURCE>` | Query a source in place of the store              |
| `--scan <SCAN>`         | Query within a scan                               |

`--scan` takes a scan ID or unique prefix. The session tables then hold the
scan's sessions at the versions it read, and the scan tables hold only that
scan's tasks, notes, and issues.

`--system` adds the columns programs use to render or address a row, such as
`id_display` and `id_prefix`, which session tables hide by default.

`--source` names what the query reads in place of the store. The query reads
that source and nothing of the store. A source is read and never written.

| Source          | What the query reads                                 |
| --------------- | ---------------------------------------------------- |
| `gage:<path>`   | The Gage store at `<path>`, with every table         |
| `claude:<path>` | The Claude Code sessions under `<path>/projects`     |
| `claude:`       | The Claude Code sessions in the default location     |

A session directory such as `claude:` holds native sessions, whether or not they
are in the store. It has the `session`, `entry`, `message`, `tool_use`, and
`tool_use_file` tables with the same columns the store's tables have, and the
`blame` and `message_text` table functions. No other table is present. Rows are
keyed by the native session ID. In the `session` table, `modified` is the
session file's modification time, and `created` and `origin` are null.

```cli
gage query --source claude: "SELECT id, title FROM session ORDER BY modified DESC LIMIT 10"
```

`--scan` applies to a store. It works with a `gage:` source and is refused with
a session directory.

In the interactive prompt, `\d` lists tables, `\d <table>` shows a table's
columns, `\df` lists table functions, and `\?` shows all prompt commands.
