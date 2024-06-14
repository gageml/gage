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
      -c, --config      Show only config.
      -s, --summary     Show only summary.
      -o, --output      Show only output.
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

    >>> run("gage show")  # +parse +panel +diff -windows
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
                               Config
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

Note there is surprising behavior on Windows for the show command and
these tests. The right aligned size columns shows blank when run here.

    >>> run("gage show")  # +parse +windows
    {}
                               Files
    | name             |type                |                 |
    | -----------------|--------------------|---------------- |
    | gage.toml        |source code         |                 |
    | hello.py         |source code         |                 |
    | -----------------|--------------------|---------------- |
    | 2 files          |                    |         total:  |
    ⤶
                              Output
    | Hello Gage                                              |
    <0>

When run using a simple terminal, the tests show the size values here.

    >>> run("gage show", env={"TERM": "UNKNOWN"})  # +parse +windows
    {}
    +------------------------- Files -------------------------+
    | name           |type               |               size |
    | ---------------+-------------------+------------------- |
    | gage.toml      |source code        |              153 B |
    | hello.py       |source code        |               41 B |
    | ---------------+-------------------+------------------- |
    | 2 files        |                   |       total: 194 B |
    +---------------------------------------------------------+
    +------------------------ Output -------------------------+
    | Hello Gage                                              |
    +---------------------------------------------------------+
    <0>

Limit files using `--limit-files`. The limit is 50 by default to avoid
huge lists. This can be increased or decreased as needed.

    >>> run("gage show --limit-files 1")  # +parse -windows
    {}
    | name                        |type         |         size |
    | ----------------------------|-------------|------------- |
    | gage.toml                   |source code  |        143 B |
    | ...                         |...          |          ... |
    | ----------------------------|-------------|------------- |
    | truncated (1 of 2 files)    |             | total: 181 B |
    {}
    <0>

Show files.

    >>> run("gage show --files")  # -windows
    | name      | type        |  size |
    |-----------|-------------|-------|
    | gage.toml | source code | 143 B |
    | hello.py  | source code |  38 B |
    <0>

    >>> run("gage show --files")  # +windows
    | name      | type        |  size |
    |-----------|-------------|-------|
    | gage.toml | source code | 153 B |
    | hello.py  | source code |  41 B |
    <0>

Show output.

    >>> run("gage show --output")
    Hello Gage
    <0>

## Summary Example

    >>> use_example("summary")

    >>> run("gage run default -y")
    Writing summary to summary.json
    <0>

    >>> run("gage show")  # +parse +panel
    {}
                               Config
    | fake_speed  0.1                                          |
    | type        example                                      |
    ⤶
                              Summary
    | name         |value             |type                    |
    | -------------|------------------|----------------------- |
    | type         |example           |attribute               |
    | speed        |0.1               |metric                  |
    {}
    <0>

    >>> run("gage show --config")
    | fake_speed | 0.1     |
    | type       | example |
    <0>

    >>> run("gage show --summary")
    | name  | value   | type      |
    |-------|---------|-----------|
    | type  | example | attribute |
    | speed | 0.1     | metric    |
    <0>

## Errors

Mutually exclusive options:

    >>> run("gage show --files --summary")
    files and summary cannot be used together.
    ⤶
    Try 'gage show -h' for help.
    <1>

    >>> run("gage show --summary --output")
    summary and output cannot be used together.
    ⤶
    Try 'gage show -h' for help.
    <1>

    >>> run("gage show --output --files")
    output and files cannot be used together.
    ⤶
    Try 'gage show -h' for help.
    <1>
