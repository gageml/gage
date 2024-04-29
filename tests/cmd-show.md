---
test-options: +skip=WINDOWS_FIX  # file size calc issue on windows
---

# `show` command

    >>> run("gage show -h")  # +diff
    Usage: gage show [options] [run]
    ⤶
      Show information about a run.
    ⤶
    Arguments:
      [run]  Run to show information for. Value may be an
             index number, run ID, or run name.
    ⤶
    Options:
      -w, --where expr  Limit available runs to show.
      --limit-files N   Limit files shown. Default is 50.
                        Ignored if --files is specified. Use
                        --all-files to bypass this limit.
      --all-files       Show all files. --limit-files is
                        ignored.
      --summary         Show only summary.
      -f, --files       Show only files. When used, all files
                        are show.
      -h, --help        Show this message and exit.
    <0>

## Hello Example

Generate a run.

    >>> use_example("hello")
    >>> run("gage run -q -y")
    <0>

Show the run.

    >>> run("gage show")  # +parse -space +diff
    {:run_id}
    | hello:hello                                    completed |
    ⤶
                             Attributes
    | id         {:run_id}                                     |
    | name       {:run_name}                                   |
    | started    {:datetime}                                   |
    | stopped    {:datetime}                                   |
    | location   {:path}                                       |
    | project    {:path}/examples/hello                        |
    | exit_code  0                                             |
    ⤶
                           Configuration
    | name  Gage                                               |
    ⤶
                               Files
    | name            |type               |               size |
    | ----------------|-------------------|------------------- |
    | gage.toml       |source code        |              143 B |
    | hello.py        |source code        |               38 B |
    | ----------------|-------------------|------------------- |
    | 2 files         |                   |       total: 181 B |
    ⤶
                               Output
    | Hello Gage                                               |
    <0>

Limit files using `--limit-files`. The limit is 50 by default to avoid
huge lists. This can be increased or decreased as needed.

    >>> run("gage show --limit-files 1")  # +wildcard=///
    ///
    | name                        |type         |         size |
    | ----------------------------|-------------|------------- |
    | gage.toml                   |source code  |        143 B |
    | ...                         |...          |          ... |
    | ----------------------------|-------------|------------- |
    | truncated (1 of 2 files)    |             | total: 181 B |
    ///
    <0>


Show files.

    >>> run("gage show --files")
    | name      | type        |  size |
    |-----------|-------------|-------|
    | gage.toml | source code | 143 B |
    | hello.py  | source code |  38 B |
    <0>

## Summary Example

    >>> use_example("summary")

    >>> run("gage run default -y")
    Writing summary to summary.json
    <0>

    >>> run("gage show")  # +wildcard=///
    ///
                              Summary
    | name         |value             |type                    |
    | -------------|------------------|----------------------- |
    | type         |example           |attribute               |
    | speed        |0.1               |metric                  |
    ///
    <0>

    >>> run("gage show --summary")
    | name  | value   | type      |
    |-------|---------|-----------|
    | type  | example | attribute |
    | speed | 0.1     | metric    |
    <0>
