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
      -f, --files  Show run files.
      -h, --help   Show this message and exit.
    <0>

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
    |                 |                   |       total: 181 B |
    ⤶
                               Output
    | Hello Gage                                               |
    <0>

Show files.

    >>> run("gage show --files")
    | name      | type        |  size |
    |-----------|-------------|-------|
    | gage.toml | source code | 143 B |
    | hello.py  | source code |  38 B |
    <0>
