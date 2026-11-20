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
| `-N, --native <SOURCE>`     | Query a session source in place of the store      |
| `-R, --stored-remote <NAME>` | Query the objects fetched from a remote in place of the store |
| `--scan <SCAN>`         | Query within a scan                               |

`--scan` takes a scan ID or unique prefix. The session tables then hold the
scan's sessions at the versions it read, and the scan tables hold only that
scan's tasks, notes, and issues.

`--system` adds the columns programs use to render or address a row, such as
`id_display` and `id_prefix`, which session tables hide by default.

`--native` names a session source that the query reads in place of the store.
The query reads that source and nothing of the store. A source is read and
never written.

| Source          | What the query reads                                 |
| --------------- | ---------------------------------------------------- |
| `claude:<path>` | The Claude Code sessions under `<path>/projects`     |
| `claude:`       | The Claude Code sessions in the default location     |
| `codex:<path>`  | The Codex sessions under `<path>/sessions`           |
| `codex:`        | The Codex sessions in the default location           |
| `pi:<path>`     | The Pi sessions under `<path>/sessions`, or `<path>` |
| `pi:`           | The Pi sessions in the default location              |

A scheme alone names the same source as the scheme with a colon: `claude` is
`claude:`. A path with no scheme is an error.

A session directory such as `claude:` holds native sessions, whether or not they
are in the store. It has the `session`, `entry`, `message`, `tool_use`, and
`tool_use_file` tables with the same columns the store's tables have, and the
`blame` and `message_text` table functions. No other table is present. Rows are
keyed by the native session ID. In the `session` table, `modified` is the
session file's modification time, and `created` and `origin` are null.

```cli
gage query --native claude: "SELECT id, title FROM session ORDER BY modified DESC LIMIT 10"
```

`--stored-remote` names a remote of the store, as listed by
[`gage remote`](/docs/commands/remote). The query reads the objects that
[`gage fetch`](/docs/commands/fetch) brought from that remote and nothing of the
store's own objects. Fetched objects are kept apart from the store's own until
[`gage merge`](/docs/commands/merge) merges them. Every table is present and
holds the remote's sessions, notes, issues, scans, and datasets. The query
fetches nothing: the rows are the remote as of the last `gage fetch`. Nothing is
written. A remote that nothing was fetched from is an error that names
`gage fetch`.

```cli
gage fetch acme --all
gage query --stored-remote acme "SELECT id, title FROM session ORDER BY modified DESC LIMIT 10"
```

`--native` and `--stored-remote` exclude each other.

`--scan` applies to a store. It works with `--stored-remote` and is refused with
`--native`.

In the interactive prompt, `\d` lists tables, `\d <table>` shows a table's
columns, `\df` lists table functions, and `\?` shows all prompt commands.
