---
title: gage test
description:
  Command reference for gage test, including subcommands to run, list, view,
  annotate, and delete test runs
---

:::note

This command supports Gage development. Normal use does not require it. It
runs Gage's own test suites, including scanner tests.

:::

The `gage test` command runs and views tests.

```cli
gage test <COMMAND>
```

## test run

Run tests.

```cli
gage test run [OPTIONS]
```

| Option                 | Description                                          |
| ---------------------- | ---------------------------------------------------- |
| `-t, --test <TEST>`    | Test to run (repeatable)                             |
| `-a, --all`            | Run all tests                                        |
| `-l, --list-tests`     | Print selected tests and exit                        |
| `-m, --model <MODEL>`  | Model for tests (default: `sonnet`)                  |
| `-e, --effort <EFFORT>` | Effort level for tests: `low` (default), `medium`, `high`, `xhigh`, `max` |
| `-n, --note <NOTE>`    | Note recorded with the run                           |
| `-d, --tests-dir <DIR>` | Load tests from a directory instead of the repo's tests |
| `-j, --jobs <N>`       | Concurrent tests (default: 4)                        |
| `--jobs-samples <N>`   | Concurrent samples within a scanner test (default: 4) |
| `--judge-model <MODEL>` | Judge model for scanner tests (default: `sonnet`)   |
| `-y, --yes`            | Run without being prompted                           |

A test selection is a name or pattern. `suite/test` matches one test. A bare
token matches that test-id in any suite, or every test in a suite of that
name. `*` matches within a segment but does not cross `/`.

The run note is stored in `manifest.json` and shown in `gage test list`. It is
useful for labeling what you were varying.

A tests directory holds suite `*.toml` files and a `fixtures/` subdir. Use it
for ad hoc tests staged outside source control, for example under
`~/.gage/tmp/tests`.

## test list

List test runs.

```cli
gage test list [OPTIONS]
```

| Option                | Description                                          |
| --------------------- | ---------------------------------------------------- |
| `-m, --more`          | Show more items. Repeat to show more per use.        |
| `-a, --all`           | Show all items                                       |
| `-n, --limit <LIMIT>` | Limit the number of items shown (default: 20)        |
| `-s, --since <SINCE>` | Filter to runs started within this duration (e.g. `1h`, `30m`, `7d`) |

## test view

View a test run report.

```cli
gage test view [RUN_ID]
```

Run IDs are UUIDs and accept unique prefixes.

## test note

Set a test run note.

```cli
gage test note [OPTIONS] <RUN_ID>
```

| Option                | Description              |
| --------------------- | ------------------------ |
| `-m, --message <TEXT>` | Note text               |
| `-d, --delete`        | Delete the note          |
| `-y, --yes`           | Skip confirmation prompt |

## test delete

Delete one or more test runs.

```cli
gage test delete [OPTIONS] [RUN_IDS]...
```

Each run ID must match exactly one run.

| Option      | Description              |
| ----------- | ------------------------ |
| `-y, --yes` | Skip confirmation prompt |
